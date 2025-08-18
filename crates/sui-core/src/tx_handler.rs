use std::{fs, sync::Arc};

use anyhow::Result;
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    GenericNamespaced, ListenerOptions,
};
use once_cell::sync::Lazy as OnceCellLazy;
use serde::{Deserialize, Serialize};
use sui_json_rpc_types::SuiEvent;
use sui_types::effects::TransactionEffects;

use tokio::{
    io::AsyncWriteExt,
    runtime::{Builder, Runtime},
    sync::Mutex,
};
use tracing::error;

pub const TX_SOCKET_PATH: &str = "/tmp/sui_tx.sock";

// 全局复用一个多线程 Runtime（用于发送任务）
static TOKIO_RT: OnceCellLazy<Runtime> = OnceCellLazy::new(|| {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("create Tokio runtime")
});

#[derive(Debug, Serialize, Deserialize)]
pub struct TxOutMsg {
    /// events 仍然用 JSON 字节
    pub sui_events_json: Vec<u8>,
    /// effects 用 BCS 字节，稳定而且更小
    pub effect_bcs: Vec<u8>,
}

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
        // 清理残留的 socket 文件
        let _ = fs::remove_file(path);

        let name = path
            .to_ns_name::<GenericNamespaced>()
            .expect("Invalid tx socket path");
        let opts = ListenerOptions::new().name(name);
        let listener = opts.create_tokio().expect("Failed to bind tx socket");

        let conns = Arc::new(Mutex::new(vec![]));
        let conns_clone = conns.clone();

        // 用当前运行时的 tokio::spawn；确保在 Tokio runtime 内调用 new()
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(conn) => conns_clone.lock().await.push(conn),
                    Err(_e) => continue,
                }
            }
        });

        Self {
            path: path.to_string(),
            conns,
        }
    }

    /// 发送一条长度前缀的 bincode 消息；失败的连接会被剔除
    pub async fn send_msg(&self, msg: &TxOutMsg) -> Result<()> {
        let payload = bincode::serialize(msg)?;
        let len = (payload.len() as u32).to_be_bytes();

        let mut conns = self.conns.lock().await;
        let mut alive = Vec::with_capacity(conns.len());

        while let Some(mut conn) = conns.pop() {
            let res: Result<()> = async {
                conn.write_all(&len).await?;
                conn.write_all(&payload).await?;
                Ok(())
            }
            .await;

            if res.is_ok() {
                alive.push(conn);
            }
        }

        *conns = alive;
        Ok(())
    }

    /// 异步发送（不阻塞调用方）
    pub fn send_sync_msg(&self, msg: TxOutMsg) -> Result<()> {
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


    /// 便捷函数：从 `TransactionOutputs` 构造 wire 数据并异步发送
    pub fn send_sync(
        &self,
        sui_events: Vec<SuiEvent>,
        effect: TransactionEffects,
    ) -> Result<()> {

        // 🚨 关键优化：只有在有MEV客户端连接时才发送数据
        if !self.has_connections_sync() {
            return Ok(()); // 没有连接，直接返回，避免内存堆积
        }


        let msg = TxOutMsg {
            sui_events_json: serde_json::to_vec(&sui_events)?,
            effect_bcs: bcs::to_bytes(&effect)?, // 关键：effects -> BCS bytes
        };
        self.send_sync_msg(msg)
    }
}
