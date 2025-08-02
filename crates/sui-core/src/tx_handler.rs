use std::{fs, sync::Arc};

use anyhow::Result;
use futures::future::Lazy;
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    GenericNamespaced, ListenerOptions,
};
use once_cell::sync::Lazy as OnceCellLazy;
use sui_json_rpc_types::SuiEvent;
use sui_types::effects::TransactionEffects;
use tokio::{
    io::AsyncWriteExt,
    runtime::{Builder, Runtime},
    sync::Mutex,
};
use tracing::{error};

pub const TX_SOCKET_PATH: &str = "/tmp/sui_tx.sock";

// 全局复用一个多线程 Runtime
static TOKIO_RT: OnceCellLazy<Runtime> = OnceCellLazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("create Tokio runtime")
});

#[derive(Clone)]
pub struct TxHandler {
    path: String,
    conns: Arc<Mutex<Vec<Stream>>>,
}

impl Default for TxHandler {
    fn default() -> Self {
        Self::new(TX_SOCKET_PATH)
    }
}

impl Drop for TxHandler {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl TxHandler {
    pub fn new(path: &str) -> Self {
        let _ = fs::remove_file(path);

        let name = path
            .to_ns_name::<GenericNamespaced>()
            .expect("Invalid tx socket path");
        let opts = ListenerOptions::new().name(name);
        let listener = opts.create_tokio().expect("Failed to bind tx socket");
        let conns = Arc::new(Mutex::new(vec![]));
        let conns_clone = conns.clone();

        tokio::spawn(async move {
            loop {
                let conn = match listener.accept().await {
                    Ok(c) => c,
                    _err => {
                        continue;
                    }
                };

                conns_clone.lock().await.push(conn);
            }
        });

        Self {
            path: path.to_string(),
            conns,
        }
    }

    pub async fn send_tx_effects_and_events(
        &self,
        effects: &TransactionEffects,
        events: Vec<SuiEvent>,
    ) -> Result<()> {
        // Serialize effects and events separately
        let effects_bytes = bincode::serialize(effects)?;
        let events_bytes = serde_json::to_vec(&events)?;

        // Get lengths as BE bytes
        let effects_len_bytes = (effects_bytes.len() as u32).to_be_bytes();
        let events_len_bytes = (events_bytes.len() as u32).to_be_bytes();

        let mut conns = self.conns.lock().await;
        let mut active_conns = Vec::new();

        while let Some(mut conn) = conns.pop() {
            let result: Result<()> = async {
                // Write effects length and data
                conn.write_all(&effects_len_bytes).await?;
                conn.write_all(&effects_bytes).await?;

                // Write events length and data
                conn.write_all(&events_len_bytes).await?;
                conn.write_all(&events_bytes).await?;
                Ok(())
            }
            .await;

            if result.is_ok() {
                active_conns.push(conn);
            }
        }

        *conns = active_conns;

        Ok(())
    }

    /// 检查是否有活跃的MEV客户端连接
    pub async fn has_active_connections(&self) -> bool {
        let conns = self.conns.lock().await;
        !conns.is_empty()
    }

    /// 同步检查连接状态（非阻塞）
    pub fn has_connections_sync(&self) -> bool {
        // 使用try_lock避免阻塞，如果锁被占用则假设有连接
        match self.conns.try_lock() {
            Ok(conns) => !conns.is_empty(),
            Err(_) => true, // 锁被占用，保守地假设有连接
        }
    }

    pub fn send_sync(&self, effects: &TransactionEffects, events: Vec<SuiEvent>) -> Result<()> {
        // 🚨 关键优化：只有在有MEV客户端连接时才发送数据
        if !self.has_connections_sync() {
            return Ok(()); // 没有连接，直接返回，避免内存堆积
        }

        // 克隆一份数据到 async block
        let effects = effects.clone();
        let events = events.clone();
        let handler = self.clone();

        // 在全局 runtime 上 spawn 一个 task，然后立刻返回
        TOKIO_RT.spawn(async move {
            if let Err(e) = handler.send_tx_effects_and_events(&effects, events).await {
                error!("IPC send failed: {:?}", e);
            }
        });

        Ok(())
    }
}
