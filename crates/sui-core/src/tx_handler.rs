use std::{fs, sync::Arc};

use anyhow::Result;
use interprocess::local_socket::{
    tokio::{prelude::*, Stream},
    GenericNamespaced, ListenerOptions,
};
use serde::{Deserialize, Serialize};
use sui_json_rpc_types::SuiEvent;
use sui_types::effects::TransactionEffects;

use tokio::{
    io::AsyncWriteExt,
    sync::broadcast,
    time::{self, Duration},
};
use tracing::{debug, error};

pub const TX_SOCKET_PATH: &str = "/tmp/sui_tx.sock";

#[derive(Debug, Serialize, Deserialize)]
pub struct TxOutMsg {
    /// events 仍然用 JSON 字节
    pub sui_events_json: Vec<u8>,
    /// effects 用 BCS 字节，稳定而且更小
    pub effect_bcs: Vec<u8>,
}

/// 采用广播模型：发送端只需一次序列化；每个连接有独立写协程
#[derive(Clone)]
pub struct TxHandler {
    path: String,
    tx: broadcast::Sender<Arc<[u8]>>, // 广播帧（已经包含长度前缀）
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

        // 广播队列：容量代表最多缓冲多少帧；慢消费者会触发 Lagged 而不是拖累全局
        const BCAST_CAP: usize = 1024;
        let (tx, _rx) = broadcast::channel::<Arc<[u8]>>(BCAST_CAP);

        // 绑定监听
        let name = path
            .to_ns_name::<GenericNamespaced>()
            .expect("Invalid tx socket path");
        let opts = ListenerOptions::new().name(name);
        let listener = opts.create_tokio().expect("Failed to bind tx socket");

        // 接入循环：每个新连接启动一个独立写协程
        let tx_clone = tx.clone();
        let path_string = path.to_string();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(mut conn) => {
                        debug!("tx socket accepted: {path_string}");
                        let mut rx = tx_clone.subscribe();

                        // 每连接独立写协程
                        tokio::spawn(async move {
                            // 可调写超时；超时则断开慢连接，避免拖累
                            const WRITE_TIMEOUT_MS: u64 = 10;

                            loop {
                                match rx.recv().await {
                                    Ok(bytes) => {
                                        // bytes 已经是 [len(4B) | payload] 的完整帧，直接写
                                        let write_res = time::timeout(
                                            Duration::from_millis(WRITE_TIMEOUT_MS),
                                            conn.write_all(bytes.as_ref()),
                                        )
                                        .await;

                                        match write_res {
                                            Ok(Ok(())) => { /* 写入成功 */ }
                                            Ok(Err(e)) => {
                                                debug!("tx socket write error, drop conn: {e:?}");
                                                break;
                                            }
                                            Err(_elapsed) => {
                                                debug!("tx socket write timeout, drop slow conn");
                                                break;
                                            }
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                        // 该连接落后了，跳过旧消息继续
                                        debug!("tx socket lagged, skipped {skipped} messages");
                                        continue;
                                    }
                                    Err(broadcast::error::RecvError::Closed) => {
                                        // 广播关闭 -> 结束
                                        break;
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        // accept 失败，短暂休眠后重试，避免忙循环
                        debug!("tx socket accept error: {e:?}");
                        time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        });

        Self {
            path: path.to_string(),
            tx,
        }
    }

    /// 内部：组装完整帧 [len(4B, BE) | payload]，避免每连接重复做 framing
    fn build_frame(msg: &TxOutMsg) -> Result<Arc<[u8]>> {
        let payload = bincode::serialize(msg)?;
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(&payload);
        Ok(Arc::<[u8]>::from(framed.into_boxed_slice()))
    }

    /// 发送一条长度前缀的 bincode 消息（异步签名保留，内部为 O(1) 非阻塞）
    pub async fn send_msg(&self, msg: &TxOutMsg) -> Result<()> {
        let frame = Self::build_frame(msg)?;
        // 广播 send() 是同步 O(1)，只在“无订阅者”时返回 Err，可忽略
        let _ = self.tx.send(frame);
        Ok(())
    }

    /// 异步发送（不阻塞调用方）：同上，不过接受 by-value
    pub fn send_sync_msg(&self, msg: TxOutMsg) -> Result<()> {
        let frame = Self::build_frame(&msg)?;
        let _ = self.tx.send(frame);
        Ok(())
    }

    /// 便捷函数：从 `TransactionOutputs` 构造 wire 数据并异步发送
    pub fn send_sync(&self, sui_events: Vec<SuiEvent>, effect: TransactionEffects) -> Result<()> {
        let msg = TxOutMsg {
            sui_events_json: serde_json::to_vec(&sui_events)?,
            effect_bcs: bcs::to_bytes(&effect)?, // 关键：effects -> BCS bytes
        };
        self.send_sync_msg(msg)
    }
}
