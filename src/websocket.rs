use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use std::sync::RwLock;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, error, info, warn};

const GATEWAY_URL: &str = "wss://api.sgroup.qq.com/websocket/";
const MAX_CONCURRENT_HANDLERS: usize = 10;
const MAX_BACKOFF_SECS: u64 = 30;
const HEARTBEAT_ACK_TIMEOUT_FACTOR: u64 = 3;

#[derive(Error, Debug)]
pub enum WebSocketError {
    #[error("WebSocket连接错误: {0}")]
    Connection(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("JSON解析错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("发送错误: {0}")]
    Send(String),

    #[error("接收错误: {0}")]
    Receive(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum OpCode {
    Dispatch = 0,
    Heartbeat = 1,
    Identify = 2,
    Resume = 6,
    Reconnect = 7,
    InvalidSession = 9,
    Hello = 10,
    HeartbeatAck = 11,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DisconnectReason {
    Reconnect,
    InvalidSession,
    HeartbeatTimeout,
    NetworkError,
}

#[derive(Debug, Serialize)]
struct IdentifyPayload {
    token: String,
    intents: i32,
    shard: [i32; 2],
    properties: Properties,
}

#[derive(Debug, Serialize)]
struct Properties {
    #[serde(rename = "$os")]
    os: String,
    #[serde(rename = "$browser")]
    browser: String,
    #[serde(rename = "$device")]
    device: String,
}

#[derive(Debug, Serialize)]
struct ResumePayload {
    token: String,
    session_id: String,
    seq: i32,
}

#[derive(Debug, Serialize)]
struct WebSocketMessage<T> {
    op: OpCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    d: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    s: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    t: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HelloData {
    heartbeat_interval: u64,
}

#[derive(Debug, Deserialize)]
struct ReadyData {
    #[allow(dead_code)]
    version: i32,
    session_id: String,
    user: User,
    #[allow(dead_code)]
    shard: [i32; 2],
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ResumedData {
    trace: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub bot: bool,
}

#[derive(Debug, Deserialize)]
pub struct MessageAuthor {
    pub id: String,
    pub username: Option<String>,
    pub bot: bool,
    pub user_openid: Option<String>,
    pub union_openid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MessageScene {
    pub source: String,
    pub ext: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct MessageEvent {
    pub id: String,
    pub author: MessageAuthor,
    pub content: String,
    pub message_type: i32,
    pub timestamp: String,
    pub message_scene: Option<MessageScene>,
}

pub trait WebSocketHandler: Send + Sync {
    fn on_message(
        &self,
        message: MessageEvent,
        client: Arc<WebSocketClient>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

#[derive(Debug, Deserialize)]
struct IncomingMessage {
    op: OpCode,
    #[serde(default)]
    d: Option<Box<serde_json::value::RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    s: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    t: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WebSocketConfig {
    pub token: Arc<str>,
    pub intents: i32,
    pub shard: [i32; 2],
}

impl WebSocketConfig {
    pub fn new(token: Arc<str>, intents: i32) -> Self {
        Self {
            token,
            intents,
            shard: [0, 1],
        }
    }

    pub fn with_shard(mut self, shard_id: i32, shard_count: i32) -> Self {
        self.shard = [shard_id, shard_count];
        self
    }
}

pub struct WebSocketClient {
    config: WebSocketConfig,
    http_client: Arc<Client>,
    session_id: RwLock<Option<String>>,
    last_sequence: Arc<AtomicI32>,
}

impl WebSocketClient {
    pub fn new(config: WebSocketConfig, http_client: Arc<Client>) -> Self {
        Self {
            config,
            http_client,
            session_id: RwLock::new(None),
            last_sequence: Arc::new(AtomicI32::new(0)),
        }
    }

    pub fn token(&self) -> &Arc<str> {
        &self.config.token
    }

    pub fn http_client(&self) -> &Arc<Client> {
        &self.http_client
    }

    pub fn get_session_id(&self) -> Option<String> {
        self.session_id.read().ok().and_then(|g| g.clone())
    }

    pub fn get_last_sequence(&self) -> i32 {
        self.last_sequence.load(Ordering::Relaxed)
    }

    fn clear_session(&self) {
        if let Ok(mut guard) = self.session_id.write() {
            *guard = None;
        }
        self.last_sequence.store(0, Ordering::Relaxed);
    }

    pub async fn run(
        self: Arc<Self>,
        handler: Arc<dyn WebSocketHandler>,
        cancellation_token: CancellationToken,
    ) -> Result<(), WebSocketError> {
        let mut backoff_secs: u64 = 1;

        loop {
            tokio::select! {
                result = Arc::clone(&self).connect_inner(Arc::clone(&handler), cancellation_token.clone()) => {
                    match result {
                        Ok(reason) => {
                            let can_resume = reason != DisconnectReason::InvalidSession;
                            if !can_resume {
                                self.clear_session();
                            }

                            info!(
                                reason = ?reason,
                                can_resume = can_resume,
                                backoff_secs = backoff_secs,
                                "连接断开，准备重连"
                            );

                            tokio::select! {
                                _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {
                                    backoff_secs = 1;
                                }
                                _ = cancellation_token.cancelled() => {
                                    info!("收到关机信号，停止重连");
                                    return Ok(());
                                }
                            }
                        }
                        Err(e) => {
                            self.clear_session();

                            error!(error = %e, "连接失败，准备重连");

                            tokio::select! {
                                _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {
                                    backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                                }
                                _ = cancellation_token.cancelled() => {
                                    info!("收到关机信号，停止重连");
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
                _ = cancellation_token.cancelled() => {
                    info!("收到关机信号，停止重连");
                    return Ok(());
                }
            }
        }
    }

    async fn connect_inner(
        self: Arc<Self>,
        handler: Arc<dyn WebSocketHandler>,
        cancellation_token: CancellationToken,
    ) -> Result<DisconnectReason, WebSocketError> {
        info!(url = %GATEWAY_URL, "开始连接WebSocket");

        let (ws_stream, _) = connect_async(GATEWAY_URL).await?;
        info!("WebSocket连接成功");

        let (mut write, mut read) = ws_stream.split();

        let hello_msg = read
            .next()
            .await
            .ok_or_else(|| WebSocketError::Receive("未收到Hello消息".to_string()))??;

        let hello_text = hello_msg.to_text()?;
        debug!(message = %hello_text, "收到Hello消息");

        let hello: IncomingMessage = serde_json::from_str(hello_text)?;
        if hello.op != OpCode::Hello {
            return Err(WebSocketError::Receive(format!(
                "期望OpCode 10, 收到 {:?}",
                hello.op
            )));
        }

        let hello_data: HelloData = serde_json::from_str(
            hello
                .d
                .ok_or_else(|| WebSocketError::Receive("Hello消息缺少数据".to_string()))?
                .get(),
        )?;

        let heartbeat_interval = hello_data.heartbeat_interval;
        info!(heartbeat_interval = heartbeat_interval, "收到心跳间隔");

        let session_id = self.session_id.read().ok().and_then(|g| g.clone());
        let seq = self.last_sequence.load(Ordering::Relaxed);

        if let Some(sid) = session_id
            && seq > 0
        {
            let resume = WebSocketMessage {
                op: OpCode::Resume,
                d: Some(ResumePayload {
                    token: format!("QQBot {}", self.config.token),
                    session_id: sid,
                    seq,
                }),
                s: None,
                t: None,
            };

            let resume_json = serde_json::to_string(&resume)?;
            debug!(message = %resume_json, "发送Resume消息");
            write.send(WsMessage::Text(resume_json.into())).await?;
        } else {
            let identify = WebSocketMessage {
                op: OpCode::Identify,
                d: Some(IdentifyPayload {
                    token: format!("QQBot {}", self.config.token),
                    intents: self.config.intents,
                    shard: self.config.shard,
                    properties: Properties {
                        os: std::env::consts::OS.to_string(),
                        browser: "qq-bot".to_string(),
                        device: "qq-bot".to_string(),
                    },
                }),
                s: None,
                t: None,
            };

            let identify_json = serde_json::to_string(&identify)?;
            debug!(message = %identify_json, "发送Identify消息");
            write.send(WsMessage::Text(identify_json.into())).await?;
        }

        let auth_msg = read
            .next()
            .await
            .ok_or_else(|| WebSocketError::Receive("未收到鉴权响应".to_string()))??;

        let auth_text = auth_msg.to_text()?;

        let auth_incoming: IncomingMessage = serde_json::from_str(auth_text)?;
        match auth_incoming.op {
            OpCode::InvalidSession => {
                warn!("Resume失败，收到InvalidSession");
                self.clear_session();
                return Ok(DisconnectReason::InvalidSession);
            }
            OpCode::Dispatch if auth_incoming.t.as_deref() == Some("READY") => {
                let ready_data: ReadyData = serde_json::from_str(
                    auth_incoming
                        .d
                        .ok_or_else(|| WebSocketError::Receive("Ready消息缺少数据".to_string()))?
                        .get(),
                )?;

                if let Ok(mut guard) = self.session_id.write() {
                    *guard = Some(ready_data.session_id.clone());
                }

                info!(
                    session_id = %ready_data.session_id,
                    user_id = %ready_data.user.id,
                    username = %ready_data.user.username,
                    "Identify鉴权成功"
                );
            }
            OpCode::Dispatch if auth_incoming.t.as_deref() == Some("RESUMED") => {
                info!("Resume鉴权成功，恢复会话");
            }
            _ => {
                return Err(WebSocketError::Receive(format!(
                    "期望READY/RESUMED事件, 收到 op={:?} t={:?}",
                    auth_incoming.op, auth_incoming.t
                )));
            }
        }

        let (disconnect_tx, mut disconnect_rx) = mpsc::channel::<DisconnectReason>(1);
        let (event_tx, mut event_rx) = mpsc::channel::<MessageEvent>(32);
        let last_heartbeat_ack = Arc::new(AtomicI64::new(0));

        let last_sequence = Arc::clone(&self.last_sequence);
        let heartbeat_ack_clone = Arc::clone(&last_heartbeat_ack);
        let disconnect_tx_heartbeat = disconnect_tx.clone();

        let heartbeat_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(heartbeat_interval));
            let ack_timeout = Duration::from_millis(heartbeat_interval * HEARTBEAT_ACK_TIMEOUT_FACTOR);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let now = now_millis();
                        let last_ack = heartbeat_ack_clone.load(Ordering::Relaxed);
                        if last_ack > 0 && now - last_ack > ack_timeout.as_millis() as i64 {
                            warn!(
                                last_ack_ms = last_ack,
                                now_ms = now,
                                timeout_ms = ack_timeout.as_millis() as i64,
                                "心跳ACK超时"
                            );
                            let _ = disconnect_tx_heartbeat.send(DisconnectReason::HeartbeatTimeout).await;
                            break;
                        }

                        let seq = last_sequence.load(Ordering::Relaxed);
                        let heartbeat = WebSocketMessage::<i32> {
                            op: OpCode::Heartbeat,
                            d: if seq > 0 { Some(seq) } else { None },
                            s: None,
                            t: None,
                        };

                        match serde_json::to_string(&heartbeat) {
                            Ok(json) => {
                                debug!(seq = seq, "发送心跳");
                                if let Err(e) = write.send(WsMessage::Text(json.into())).await {
                                    error!(error = %e, "发送心跳失败");
                                    let _ = disconnect_tx_heartbeat.send(DisconnectReason::NetworkError).await;
                                    break;
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "序列化心跳消息失败");
                            }
                        }
                    }
                }
            }
        });

        let last_sequence_clone = Arc::clone(&self.last_sequence);
        let heartbeat_ack_reader = Arc::clone(&last_heartbeat_ack);
        let disconnect_tx_read = disconnect_tx.clone();

        let read_handle = tokio::spawn(async move {
            while let Some(msg_result) = read.next().await {
                match msg_result {
                    Ok(msg) => {
                        if let Ok(text) = msg.to_text() {
                            debug!(message = %text, "收到消息");

                            if let Ok(incoming) = serde_json::from_str::<IncomingMessage>(text) {
                                if let Some(seq) = incoming.s {
                                    last_sequence_clone.store(seq, Ordering::Relaxed);
                                }

                                match incoming.op {
                                    OpCode::HeartbeatAck => {
                                        heartbeat_ack_reader.store(now_millis(), Ordering::Relaxed);
                                        debug!("收到心跳ACK");
                                    }
                                    OpCode::Dispatch => {
                                        if let Some(t) = incoming.t.as_deref() {
                                            if t == "C2C_MESSAGE_CREATE" {
                                                #[allow(clippy::collapsible_if)]
                                                if let Some(data) = incoming.d
                                                    && let Ok(message_event) = serde_json::from_str::<MessageEvent>(data.get())
                                                    && message_event.message_type == 0 {
                                                    if let Err(e) = event_tx.send(message_event).await {
                                                        error!(error = %e, "发送事件到通道失败");
                                                    }
                                                }
                                            } else {
                                                debug!(event_type = %t, "收到非文本消息事件，忽略");
                                            }
                                        }
                                    }
                                    OpCode::Reconnect => {
                                        warn!("收到重连请求");
                                        let _ = disconnect_tx_read.send(DisconnectReason::Reconnect).await;
                                        break;
                                    }
                                    OpCode::InvalidSession => {
                                        error!("收到无效会话消息");
                                        let _ = disconnect_tx_read.send(DisconnectReason::InvalidSession).await;
                                        break;
                                    }
                                    _ => {
                                        debug!(op = ?incoming.op, "收到其他消息");
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "接收消息错误");
                        let _ = disconnect_tx_read.send(DisconnectReason::NetworkError).await;
                        break;
                    }
                }
            }
        });

        drop(disconnect_tx);

        info!("开始接收事件");

        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_HANDLERS));

        let disconnect_reason = tokio::select! {
            reason = disconnect_rx.recv() => {
                reason.unwrap_or(DisconnectReason::NetworkError)
            }
            _ = async {
                while let Some(message_event) = event_rx.recv().await {
                    info!(
                        message_id = %message_event.id,
                        author_id = %message_event.author.id,
                        content = %message_event.content,
                        "收到文本消息"
                    );

                    let handler_clone = Arc::clone(&handler);
                    let client_clone = Arc::clone(&self);
                    let permit = semaphore.clone().acquire_owned().await.unwrap();

                    tokio::spawn(async move {
                        handler_clone.on_message(message_event, client_clone).await;
                        drop(permit);
                    });
                }
            } => {
                DisconnectReason::NetworkError
            }
            _ = cancellation_token.cancelled() => {
                info!("收到关机信号，断开连接");
                DisconnectReason::NetworkError
            }
        };

        heartbeat_handle.abort();
        read_handle.abort();

        Ok(disconnect_reason)
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
