use std::{fs, sync::Arc};

use anyhow::Result;
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    GenericNamespaced, ListenerOptions,
};
use once_cell::sync::Lazy as OnceCellLazy;
use serde::{Deserialize, Serialize};
use sui_json_rpc_types::SuiEvent;
use sui_types::{
    accumulator_event::AccumulatorEvent,
    base_types::{EpochId, ObjectID},
    digests::TransactionDigest,
    effects::{TransactionEffects, TransactionEvents},
    object::Object,
    storage::{FullObjectKey, MarkerValue, ObjectKey}, transaction::SenderSignedData,
};

use tokio::{
    io::AsyncWriteExt,
    runtime::{Builder, Runtime},
    sync::Mutex,
};
use tracing::error;

use crate::transaction_outputs::TransactionOutputs;

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
    pub outputs: TxOutputsWire,
    pub sui_events_json: Vec<u8>,
}

/// 可序列化的 TransactionOutputs（wire 版）
#[derive(Debug, Serialize, Deserialize)]
pub struct TxOutputsWire {
    pub epoch: u64,
    pub tx_digest: TransactionDigest,
    pub sender_data: SenderSignedData,
    pub effects: TransactionEffects,
    pub events: TransactionEvents,
    pub accumulator_events: Vec<AccumulatorEvent>,
    pub markers: Vec<(FullObjectKey, MarkerValue)>,
    pub wrapped: Vec<ObjectKey>,
    pub deleted: Vec<ObjectKey>,

    pub written: Vec<(ObjectID, Object)>,
}

impl TxOutputsWire {
    pub fn from_outputs(epoch: u64, o: &TransactionOutputs) -> Self {
        let tx_digest = *o.transaction.digest();
        let written = o
            .written
            .iter()
            .map(|(id, obj)| (*id, obj.clone()))
            .collect();
        let acc = o.accumulator_events.lock().clone();

        let sender_data = o.transaction.data().clone();

        Self {
            epoch,
            tx_digest,
            sender_data,
            effects: o.effects.clone(),
            events: o.events.clone(),
            accumulator_events: acc,
            markers: o.markers.clone(),
            wrapped: o.wrapped.clone(),
            deleted: o.deleted.clone(),
            written,
        }
    }
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
        let conns_clone: Arc<Mutex<Vec<LocalSocketStream>>> = conns.clone();

        // 注意：这里用当前运行时的 tokio::spawn；确保在 Tokio runtime 内调用 new()
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
        let handler = self.clone();
        TOKIO_RT.spawn(async move {
            if let Err(e) = handler.send_msg(&msg).await {
                error!("IPC send failed: {:?}", e);
            }
        });
        Ok(())
    }

    /// 便捷函数：从 `TransactionOutputs` 构造 wire 数据并异步发送
    pub fn send_sync(
        &self,
        epoch: EpochId,
        outputs: &TransactionOutputs, // ← 按引用
        sui_events: Vec<SuiEvent>,
    ) -> Result<()> {
        let wire = TxOutputsWire::from_outputs(epoch as u64, outputs);
        let msg = TxOutMsg {
            outputs: wire,
            sui_events_json: serde_json::to_vec(&sui_events)?,
        };
        self.send_sync_msg(msg)
    }
}
