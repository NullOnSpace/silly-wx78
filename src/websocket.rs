use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, error, info, warn};

const GATEWAY_URL: &str = "wss://api.sgroup.qq.com/websocket/";

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

#[derive(Debug, Clone, Copy, PartialEq)]
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

impl Serialize for OpCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i32(*self as i32)
    }
}

impl<'de> Deserialize<'de> for OpCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = i32::deserialize(deserializer)?;
        match value {
            0 => Ok(OpCode::Dispatch),
            1 => Ok(OpCode::Heartbeat),
            2 => Ok(OpCode::Identify),
            6 => Ok(OpCode::Resume),
            7 => Ok(OpCode::Reconnect),
            9 => Ok(OpCode::InvalidSession),
            10 => Ok(OpCode::Hello),
            11 => Ok(OpCode::HeartbeatAck),
            _ => Err(serde::de::Error::custom(format!("Unknown OpCode: {}", value))),
        }
    }
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

#[async_trait::async_trait]
pub trait EventHandler: Send + Sync {
    async fn on_message(&self, message: MessageEvent, token: &str);
}

#[derive(Debug, Deserialize)]
struct IncomingMessage {
    op: OpCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    d: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    s: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    t: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WebSocketConfig {
    pub token: String,
    pub intents: i32,
    pub shard: [i32; 2],
}

impl WebSocketConfig {
    pub fn new(token: String, intents: i32) -> Self {
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
    session_id: Arc<tokio::sync::RwLock<Option<String>>>,
    last_sequence: Arc<AtomicI32>,
    event_handler: Option<Arc<dyn EventHandler>>,
}

impl WebSocketClient {
    pub fn new(config: WebSocketConfig) -> Self {
        Self {
            config,
            session_id: Arc::new(tokio::sync::RwLock::new(None)),
            last_sequence: Arc::new(AtomicI32::new(0)),
            event_handler: None,
        }
    }

    pub fn with_event_handler(mut self, handler: Arc<dyn EventHandler>) -> Self {
        self.event_handler = Some(handler);
        self
    }

    pub async fn connect(&self) -> Result<(), WebSocketError> {
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

        let hello_data: HelloData = serde_json::from_value(
            hello.d.ok_or_else(|| WebSocketError::Receive("Hello消息缺少数据".to_string()))?,
        )?;
        
        let heartbeat_interval = hello_data.heartbeat_interval;
        info!(heartbeat_interval = heartbeat_interval, "收到心跳间隔");

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

        let ready_msg = read
            .next()
            .await
            .ok_or_else(|| WebSocketError::Receive("未收到Ready消息".to_string()))??;

        let ready_text = ready_msg.to_text()?;
        debug!(message = %ready_text, "收到Ready消息");

        let ready: IncomingMessage = serde_json::from_str(ready_text)?;
        if ready.op != OpCode::Dispatch || ready.t.as_deref() != Some("READY") {
            return Err(WebSocketError::Receive(format!(
                "期望READY事件, 收到 {:?}",
                ready.t
            )));
        }

        let ready_data: ReadyData = serde_json::from_value(
            ready.d.ok_or_else(|| WebSocketError::Receive("Ready消息缺少数据".to_string()))?,
        )?;

        let mut session_id = self.session_id.write().await;
        *session_id = Some(ready_data.session_id.clone());
        drop(session_id);

        info!(
            session_id = %ready_data.session_id,
            user_id = %ready_data.user.id,
            username = %ready_data.user.username,
            "登录鉴权成功"
        );

        let (heartbeat_tx, mut heartbeat_rx) = mpsc::channel::<()>(1);
        let (event_tx, mut event_rx) = mpsc::channel::<serde_json::Value>(100);

        let last_sequence = Arc::clone(&self.last_sequence);
        let write_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(heartbeat_interval));
            
            loop {
                tokio::select! {
                    _ = interval.tick() => {
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
                                    break;
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "序列化心跳消息失败");
                            }
                        }
                    }
                    
                    _ = heartbeat_rx.recv() => {
                        info!("停止心跳任务");
                        break;
                    }
                }
            }
        });

        let last_sequence_read = Arc::clone(&self.last_sequence);
        let read_handle = tokio::spawn(async move {
            while let Some(msg_result) = read.next().await {
                match msg_result {
                    Ok(msg) => {
                        if let Ok(text) = msg.to_text() {
                            debug!(message = %text, "收到消息");
                            
                            if let Ok(incoming) = serde_json::from_str::<IncomingMessage>(text) {
                                if let Some(seq) = incoming.s {
                                    last_sequence_read.store(seq, Ordering::Relaxed);
                                }
                                
                                match incoming.op {
                                    OpCode::HeartbeatAck => {
                                        debug!("收到心跳ACK");
                                    }
                                    OpCode::Dispatch => {
                                        if let Some(data) = incoming.d {
                                            if let Err(e) = event_tx.send(data).await {
                                                error!(error = %e, "发送事件到通道失败");
                                            }
                                        }
                                    }
                                    OpCode::Reconnect => {
                                        warn!("收到重连请求");
                                        break;
                                    }
                                    OpCode::InvalidSession => {
                                        error!("收到无效会话消息");
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
                        break;
                    }
                }
            }
            
            let _ = heartbeat_tx.send(()).await;
        });

        info!("开始接收事件");
        
        let event_handler = self.event_handler.clone();
        let token = self.config.token.clone();

        while let Some(event) = event_rx.recv().await {
            debug!(event = ?event, "收到事件");
            
            if let Some(handler) = &event_handler {
                if let Ok(message_event) = serde_json::from_value::<MessageEvent>(event.clone()) {
                    if message_event.message_type == 0 {
                        info!(
                            message_id = %message_event.id,
                            author_id = %message_event.author.id,
                            content = %message_event.content,
                            "收到文本消息"
                        );
                        
                        let handler_clone = Arc::clone(handler);
                        let token_clone = token.clone();
                        tokio::spawn(async move {
                            handler_clone.on_message(message_event, &token_clone).await;
                        });
                    }
                }
            }
        }

        write_handle.abort();
        read_handle.abort();

        Ok(())
    }

    pub async fn get_session_id(&self) -> Option<String> {
        self.session_id.read().await.clone()
    }
}
