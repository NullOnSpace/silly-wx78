use crate::auth::AuthClient;
use crate::auth::AuthError;
use crate::guild::{ChannelInfo, Guild};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

const GATEWAY_URL: &str = "wss://api.sgroup.qq.com/websocket/";
const MAX_CONCURRENT_HANDLERS: usize = 10;
const MAX_BACKOFF_SECS: u64 = 30;
const HEARTBEAT_ACK_TIMEOUT_FACTOR: u64 = 3;

/// 事件订阅 intents 位定义（官方 WebSocket 文档值）。
///
/// 实测（docs/API_VERIFICATION.md §2.2）：`GUILDS | GUILD_MESSAGES |
/// GUILD_MESSAGE_REACTIONS | DIRECT_MESSAGE | GROUP_AND_C2C_EVENT | INTERACTION`
/// 的组合（100668929）可正常收事件；全量订阅 `2113934851` 亦可用。
pub mod intents {
    /// 频道事件：GUILD_CREATE / GUILD_UPDATE / GUILD_DELETE 等
    pub const GUILDS: i32 = 1 << 0;
    /// 频道成员事件：GUILD_MEMBER_ADD / UPDATE / REMOVE
    pub const GUILD_MEMBERS: i32 = 1 << 1;
    /// 私域频道消息事件：MESSAGE_CREATE 等
    pub const GUILD_MESSAGES: i32 = 1 << 9;
    /// 频道消息表情表态事件
    pub const GUILD_MESSAGE_REACTIONS: i32 = 1 << 10;
    /// 私信事件：DIRECT_MESSAGE_CREATE 等
    pub const DIRECT_MESSAGE: i32 = 1 << 12;
    /// 群和单聊事件：GROUP_AT_MESSAGE_CREATE / C2C_MESSAGE_CREATE
    pub const GROUP_AND_C2C_EVENT: i32 = 1 << 25;
    /// 互动事件
    pub const INTERACTION: i32 = 1 << 26;
    /// 消息审核事件（主动消息安全审核结果，实测 code=304023 场景需要）
    pub const MESSAGE_AUDIT: i32 = 1 << 27;

    /// 实测可用的常用组合（不含论坛等特殊场景）
    pub const COMMON: i32 = GUILDS
        | GUILD_MESSAGES
        | GUILD_MESSAGE_REACTIONS
        | DIRECT_MESSAGE
        | GROUP_AND_C2C_EVENT
        | INTERACTION;

    /// 全量 intents（项目历史默认值）
    pub const ALL: i32 = 2113934851;
}

#[derive(Error, Debug)]
pub enum WebSocketError {
    #[error("WebSocket连接错误: {0}")]
    Connection(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("JSON解析错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("认证错误: {0}")]
    Auth(#[from] AuthError),

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
struct User {
    id: String,
    username: String,
    #[allow(dead_code)]
    bot: bool,
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

// ---------------------------------------------------------------------------
// 频道/群/私信事件类型
//
// 实测结论（docs/API_VERIFICATION.md §2.3、§4.1）：
// - 私域机器人的频道消息事件是 MESSAGE_CREATE（全量消息），公域是 AT_MESSAGE_CREATE，
//   两者 payload 结构基本一致；
// - 频道事件的 author 没有 user_openid 字段，身份信息走 member.roles/nick；
// - 群@消息 GROUP_AT_MESSAGE_CREATE 以前被分发层丢弃，现单独建模。
// ---------------------------------------------------------------------------

/// 频道消息事件的作者（无 user_openid 字段）
#[derive(Debug, Clone, Deserialize)]
pub struct GuildMessageAuthor {
    pub id: String,
    pub username: Option<String>,
    pub avatar: Option<String>,
    #[serde(default)]
    pub bot: bool,
    pub union_openid: Option<String>,
}

/// 频道消息中的成员信息
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageMember {
    pub nick: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    pub joined_at: Option<String>,
}

/// 频道消息事件（MESSAGE_CREATE 私域全量 / AT_MESSAGE_CREATE 公域@）。
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelMessageEvent {
    /// 消息 ID。被动回复有效期 5 分钟、同一 id 最多回复 5 次。
    pub id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub content: String,
    pub timestamp: Option<String>,
    pub seq: Option<i64>,
    pub seq_in_channel: Option<String>,
    pub author: GuildMessageAuthor,
    pub member: Option<MessageMember>,
}

impl ChannelMessageEvent {
    /// 成员昵称：优先 member.nick，其次 author.username
    pub fn display_name(&self) -> &str {
        if let Some(member) = &self.member
            && let Some(nick) = member.nick.as_deref().filter(|s| !s.is_empty())
        {
            return nick;
        }
        self.author.username.as_deref().unwrap_or(&self.author.id)
    }
}

/// 仅携带 openid 的作者（群@ / 私信场景）
#[derive(Debug, Clone, Deserialize)]
pub struct OpenIdAuthor {
    pub id: String,
    pub username: Option<String>,
    pub user_openid: Option<String>,
    pub union_openid: Option<String>,
}

/// 群@机器人消息事件（GROUP_AT_MESSAGE_CREATE）
#[derive(Debug, Clone, Deserialize)]
pub struct GroupMessageEvent {
    pub id: String,
    pub group_openid: String,
    pub content: String,
    pub timestamp: Option<String>,
    pub author: OpenIdAuthor,
}

/// 频道私聊消息事件（DIRECT_MESSAGE_CREATE）。payload 与频道消息类似，
/// 但 author 带 user_openid；guild_id/channel_id 在部分场景可能缺省。
#[derive(Debug, Clone, Deserialize)]
pub struct DirectMessageEvent {
    pub id: String,
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub content: String,
    pub timestamp: Option<String>,
    pub author: OpenIdAuthor,
    pub member: Option<MessageMember>,
}

/// 频道(.guild)推送事件类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuildEventType {
    Create,
    Update,
    Delete,
}

/// 频道(.guild)变更事件。GUILD_UPDATE 触发频率不低（改名、成员进出等都会触发），
/// 若做缓存需注意去抖。
#[derive(Debug, Clone)]
pub struct GuildEvent {
    pub event_type: GuildEventType,
    pub guild: Guild,
}

/// 子频道推送事件类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelEventType {
    Create,
    Update,
    Delete,
}

/// 子频道变更事件（CHANNEL_UPDATE 比 REST 多 op_user_id 操作者字段）
#[derive(Debug, Clone)]
pub struct ChannelEvent {
    pub event_type: ChannelEventType,
    pub channel: ChannelInfo,
}

/// handler 回调的返回 future 类型
pub type HandlerFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// WebSocket 事件处理器 trait。
///
/// 各回调均有默认空实现，按需覆写即可；未识别的事件类型统一进入
/// [`WebSocketHandler::on_unknown_event`]，保证任何 payload 都不会被静默丢弃。
///
/// # 兼容性说明
/// [`WebSocketHandler::on_message`] 对应单聊 C2C 文本消息（历史行为保留不变）；
/// 频道消息请覆写 [`WebSocketHandler::on_channel_message`]。
pub trait WebSocketHandler: Send + Sync {
    /// 单聊(C2C)文本消息（message_type == 0）
    fn on_message(
        &self,
        _message: MessageEvent,
        _client: Arc<WebSocketClient>,
    ) -> HandlerFuture<'_> {
        Box::pin(async {})
    }

    /// 频道消息：私域 MESSAGE_CREATE（无需@）/ 公域 AT_MESSAGE_CREATE（需要@）
    fn on_channel_message(
        &self,
        _event: ChannelMessageEvent,
        _client: Arc<WebSocketClient>,
    ) -> HandlerFuture<'_> {
        Box::pin(async {})
    }

    /// 群@机器人消息
    fn on_group_message(
        &self,
        _event: GroupMessageEvent,
        _client: Arc<WebSocketClient>,
    ) -> HandlerFuture<'_> {
        Box::pin(async {})
    }

    /// 频道私信消息
    fn on_direct_message(
        &self,
        _event: DirectMessageEvent,
        _client: Arc<WebSocketClient>,
    ) -> HandlerFuture<'_> {
        Box::pin(async {})
    }

    /// 频道(guild)创建/更新/删除
    fn on_guild_event(
        &self,
        _event: GuildEvent,
        _client: Arc<WebSocketClient>,
    ) -> HandlerFuture<'_> {
        Box::pin(async {})
    }

    /// 子频道创建/更新/删除
    fn on_channel_event(
        &self,
        _event: ChannelEvent,
        _client: Arc<WebSocketClient>,
    ) -> HandlerFuture<'_> {
        Box::pin(async {})
    }

    /// 未识别事件类型的兜底回调（含解析失败的消息事件原始 payload）
    fn on_unknown_event(
        &self,
        _event_type: String,
        _payload: String,
        _client: Arc<WebSocketClient>,
    ) -> HandlerFuture<'_> {
        Box::pin(async {})
    }
}

/// 内部分发事件：由 read loop 按 `t` 字段解析后投入通道
pub(crate) enum DispatchEvent {
    C2cMessage(MessageEvent),
    ChannelMessage(ChannelMessageEvent),
    GroupMessage(GroupMessageEvent),
    DirectMessage(DirectMessageEvent),
    Guild(GuildEvent),
    Channel(ChannelEvent),
    Unknown { event_type: String, payload: String },
}

/// 按事件类型把 dispatch payload 解析为内部事件；任何失败都回落到 Unknown，
/// 不再像旧版那样静默丢弃非 C2C 事件。
fn decode_dispatch(event_type: &str, data: Option<&serde_json::value::RawValue>) -> DispatchEvent {
    let parsed = match event_type {
        "C2C_MESSAGE_CREATE" => data
            .and_then(|d| serde_json::from_str::<MessageEvent>(d.get()).ok())
            // 历史行为：仅分发纯文本(type=0)，媒体等其他类型进入 Unknown 兜底
            .filter(|m| m.message_type == 0)
            .map(DispatchEvent::C2cMessage),
        "MESSAGE_CREATE" | "AT_MESSAGE_CREATE" => data
            .and_then(|d| serde_json::from_str::<ChannelMessageEvent>(d.get()).ok())
            .map(DispatchEvent::ChannelMessage),
        "GROUP_AT_MESSAGE_CREATE" => data
            .and_then(|d| serde_json::from_str::<GroupMessageEvent>(d.get()).ok())
            .map(DispatchEvent::GroupMessage),
        "DIRECT_MESSAGE_CREATE" => data
            .and_then(|d| serde_json::from_str::<DirectMessageEvent>(d.get()).ok())
            .map(DispatchEvent::DirectMessage),
        "GUILD_CREATE" | "GUILD_UPDATE" | "GUILD_DELETE" => data
            .and_then(|d| serde_json::from_str::<Guild>(d.get()).ok())
            .map(|guild| {
                DispatchEvent::Guild(GuildEvent {
                    event_type: match event_type {
                        "GUILD_CREATE" => GuildEventType::Create,
                        "GUILD_UPDATE" => GuildEventType::Update,
                        _ => GuildEventType::Delete,
                    },
                    guild,
                })
            }),
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE" => data
            .and_then(|d| serde_json::from_str::<ChannelInfo>(d.get()).ok())
            .map(|channel| {
                DispatchEvent::Channel(ChannelEvent {
                    event_type: match event_type {
                        "CHANNEL_CREATE" => ChannelEventType::Create,
                        "CHANNEL_UPDATE" => ChannelEventType::Update,
                        _ => ChannelEventType::Delete,
                    },
                    channel,
                })
            }),
        _ => None,
    };

    parsed.unwrap_or_else(|| {
        if matches!(
            event_type,
            "C2C_MESSAGE_CREATE"
                | "MESSAGE_CREATE"
                | "AT_MESSAGE_CREATE"
                | "GROUP_AT_MESSAGE_CREATE"
                | "DIRECT_MESSAGE_CREATE"
                | "GUILD_CREATE"
                | "GUILD_UPDATE"
                | "GUILD_DELETE"
                | "CHANNEL_CREATE"
                | "CHANNEL_UPDATE"
                | "CHANNEL_DELETE"
        ) {
            warn!(event_type = %event_type, "事件解析失败，转入未知事件兜底");
        } else {
            debug!(event_type = %event_type, "收到未注册的事件类型");
        }
        DispatchEvent::Unknown {
            event_type: event_type.to_string(),
            payload: data.map(|d| d.get().to_string()).unwrap_or_default(),
        }
    })
}

#[derive(Debug, Deserialize)]
struct IncomingMessage {
    op: OpCode,
    #[serde(default)]
    d: Option<Box<serde_json::value::RawValue>>,
    s: Option<i32>,
    t: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WebSocketConfig {
    pub intents: i32,
    pub shard: [i32; 2],
}

impl WebSocketConfig {
    pub fn new(intents: i32) -> Self {
        Self {
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
    auth_client: Arc<AuthClient>,
    config: WebSocketConfig,
    http_client: Arc<Client>,
    session_id: RwLock<Option<String>>,
    last_sequence: Arc<AtomicI32>,
}

impl WebSocketClient {
    pub fn new(
        auth_client: Arc<AuthClient>,
        config: WebSocketConfig,
        http_client: Arc<Client>,
    ) -> Self {
        Self {
            auth_client,
            config,
            http_client,
            session_id: RwLock::new(None),
            last_sequence: Arc::new(AtomicI32::new(0)),
        }
    }

    pub fn auth_client(&self) -> &Arc<AuthClient> {
        &self.auth_client
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
            let result = Arc::clone(&self)
                .connect_inner(Arc::clone(&handler), cancellation_token.clone())
                .await;
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

        let authorization = self.auth_client.get_authorization().await?;

        if let Some(sid) = session_id
            && seq > 0
        {
            let resume = WebSocketMessage {
                op: OpCode::Resume,
                d: Some(ResumePayload {
                    token: authorization,
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
                    token: authorization,
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
        let (event_tx, mut event_rx) = mpsc::channel::<DispatchEvent>(64);
        let last_heartbeat_ack = Arc::new(AtomicI64::new(0));

        let last_sequence = Arc::clone(&self.last_sequence);
        let heartbeat_ack_clone = Arc::clone(&last_heartbeat_ack);
        let disconnect_tx_heartbeat = disconnect_tx.clone();

        let heartbeat_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(heartbeat_interval));
            let ack_timeout =
                Duration::from_millis(heartbeat_interval * HEARTBEAT_ACK_TIMEOUT_FACTOR);

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
                                            let event = decode_dispatch(t, incoming.d.as_deref());
                                            if let Err(e) = event_tx.send(event).await {
                                                error!(error = %e, "发送事件到通道失败");
                                            }
                                        }
                                    }
                                    OpCode::Reconnect => {
                                        warn!("收到重连请求");
                                        let _ = disconnect_tx_read
                                            .send(DisconnectReason::Reconnect)
                                            .await;
                                        break;
                                    }
                                    OpCode::InvalidSession => {
                                        error!("收到无效会话消息");
                                        let _ = disconnect_tx_read
                                            .send(DisconnectReason::InvalidSession)
                                            .await;
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
                        let _ = disconnect_tx_read
                            .send(DisconnectReason::NetworkError)
                            .await;
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
                while let Some(event) = event_rx.recv().await {
                    let handler_clone = Arc::clone(&handler);
                    let client_clone = Arc::clone(&self);
                    let permit = semaphore.clone().acquire_owned().await.unwrap();

                    tokio::spawn(async move {
                        match event {
                            DispatchEvent::C2cMessage(message_event) => {
                                info!(
                                    message_id = %message_event.id,
                                    author_id = %message_event.author.id,
                                    content = %message_event.content,
                                    "收到单聊文本消息"
                                );
                                handler_clone.on_message(message_event, client_clone).await;
                            }
                            DispatchEvent::ChannelMessage(event) => {
                                info!(
                                    message_id = %event.id,
                                    channel_id = %event.channel_id,
                                    author = %event.display_name(),
                                    content = %event.content,
                                    "收到频道消息"
                                );
                                handler_clone.on_channel_message(event, client_clone).await;
                            }
                            DispatchEvent::GroupMessage(event) => {
                                info!(
                                    message_id = %event.id,
                                    group_openid = %event.group_openid,
                                    content = %event.content,
                                    "收到群@消息"
                                );
                                handler_clone.on_group_message(event, client_clone).await;
                            }
                            DispatchEvent::DirectMessage(event) => {
                                info!(
                                    message_id = %event.id,
                                    author_id = %event.author.id,
                                    content = %event.content,
                                    "收到频道私信消息"
                                );
                                handler_clone.on_direct_message(event, client_clone).await;
                            }
                            DispatchEvent::Guild(event) => {
                                debug!(?event.event_type, guild_id = ?event.guild.id, "收到频道变更事件");
                                handler_clone.on_guild_event(event, client_clone).await;
                            }
                            DispatchEvent::Channel(event) => {
                                debug!(?event.event_type, channel_id = %event.channel.id, "收到子频道变更事件");
                                handler_clone.on_channel_event(event, client_clone).await;
                            }
                            DispatchEvent::Unknown {
                                event_type,
                                payload,
                            } => {
                                debug!(%event_type, "转发未知事件到兜底回调");
                                handler_clone
                                    .on_unknown_event(event_type, payload, client_clone)
                                    .await;
                            }
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::value::RawValue;

    fn raw(json: &str) -> Option<Box<RawValue>> {
        serde_json::from_str::<Box<RawValue>>(json).ok()
    }

    /// docs/API_VERIFICATION.md §4.1 的 MESSAGE_CREATE 完整样例
    const MESSAGE_CREATE_SAMPLE: &str = r#"{
        "author": {
            "avatar": "https://qqchannel-profile-1251316161.file.myqcloud.com/...",
            "bot": false,
            "id": "5688363186875882260",
            "union_openid": "",
            "username": "besiege"
        },
        "channel_id": "9478690",
        "content": "wawa",
        "guild_id": "17327925269880161597",
        "id": "08bd8ae3ad9c87cabcf00110a2c4c204380348b79abbd406",
        "member": {
            "joined_at": "2022-08-01T09:06:33+08:00",
            "nick": "besiege",
            "roles": ["4", "5", "14"]
        },
        "seq": 3,
        "seq_in_channel": "3",
        "timestamp": "2026-08-26T19:25:43+08:00"
    }"#;

    /// docs/API_VERIFICATION.md §4.2 的 CHANNEL_UPDATE 样例
    const CHANNEL_UPDATE_SAMPLE: &str = r#"{
        "application_id": "0",
        "guild_id": "17327925269880161597",
        "id": "9478690",
        "name": "活动互助",
        "op_user_id": "5688363186875882260",
        "owner_id": "5688363186875882260",
        "parent_id": "644066500",
        "permissions": "3",
        "position": 2,
        "private_type": 0,
        "speak_permission": 1,
        "sub_type": 0,
        "type": 0
    }"#;

    #[test]
    fn parses_private_domain_message_create() {
        let event: ChannelMessageEvent = serde_json::from_str(MESSAGE_CREATE_SAMPLE).unwrap();
        assert_eq!(event.id, "08bd8ae3ad9c87cabcf00110a2c4c204380348b79abbd406");
        assert_eq!(event.guild_id, "17327925269880161597");
        assert_eq!(event.channel_id, "9478690");
        assert_eq!(event.content, "wawa");
        assert_eq!(event.seq, Some(3));
        // author 无 user_openid，身份走 member
        let member = event.member.as_ref().unwrap();
        assert_eq!(member.roles, vec!["4", "5", "14"]);
        assert_eq!(event.display_name(), "besiege");
    }

    #[test]
    fn parses_group_at_message() {
        let body = r#"{
            "author": {"id": "GABC", "union_openid": "", "user_openid": "OUABC"},
            "content": "在吗",
            "group_openid": "GOPENID",
            "id": "msgid-1",
            "timestamp": "2026-08-26T19:30:00+08:00"
        }"#;
        let event: GroupMessageEvent = serde_json::from_str(body).unwrap();
        assert_eq!(event.group_openid, "GOPENID");
        assert_eq!(event.author.user_openid.as_deref(), Some("OUABC"));
    }

    #[test]
    fn decode_dispatch_routes_all_registered_types() {
        let msg = raw(MESSAGE_CREATE_SAMPLE).unwrap();
        match decode_dispatch("MESSAGE_CREATE", Some(&msg)) {
            DispatchEvent::ChannelMessage(event) => {
                assert_eq!(event.channel_id, "9478690");
            }
            _ => panic!("MESSAGE_CREATE 应分发到 ChannelMessage"),
        }

        let channel = raw(CHANNEL_UPDATE_SAMPLE).unwrap();
        match decode_dispatch("CHANNEL_UPDATE", Some(&channel)) {
            DispatchEvent::Channel(event) => {
                assert_eq!(event.event_type, ChannelEventType::Update);
                assert_eq!(
                    event.channel.op_user_id.as_deref(),
                    Some("5688363186875882260")
                );
                assert!(event.channel.is_text());
            }
            _ => panic!("CHANNEL_UPDATE 应分发到 Channel 事件"),
        }

        let guild = raw(r#"{"id":"17327925269880161597","name":"饥荒助手内测"}"#).unwrap();
        match decode_dispatch("GUILD_UPDATE", Some(&guild)) {
            DispatchEvent::Guild(event) => {
                assert_eq!(event.event_type, GuildEventType::Update);
                assert_eq!(event.guild.id, "17327925269880161597");
            }
            _ => panic!("GUILD_UPDATE 应分发到 Guild 事件"),
        }
    }

    #[test]
    fn decode_dispatch_falls_back_to_unknown_instead_of_dropping() {
        // 旧版会静默丢弃的事件：现在进入 Unknown 兜底回调
        let body = raw(r#"{"foo":"bar"}"#).unwrap();
        match decode_dispatch("SOME_NEW_EVENT", Some(&body)) {
            DispatchEvent::Unknown {
                event_type,
                payload,
            } => {
                assert_eq!(event_type, "SOME_NEW_EVENT");
                assert_eq!(payload, r#"{"foo":"bar"}"#);
            }
            _ => panic!("未注册事件应进入 Unknown"),
        }

        // 已注册但解析失败（如 C2C 媒体消息 message_type!=0）同样兜底而非丢弃
        let media = raw(
            r#"{"id":"m1","author":{"id":"u1","bot":false},"content":"","message_type":7,"timestamp":"t"}"#,
        )
        .unwrap();
        match decode_dispatch("C2C_MESSAGE_CREATE", Some(&media)) {
            DispatchEvent::Unknown { event_type, .. } => {
                assert_eq!(event_type, "C2C_MESSAGE_CREATE");
            }
            _ => panic!("非文本 C2C 消息应进入 Unknown 兜底"),
        }
    }

    #[test]
    fn intents_common_combo_matches_verified_value() {
        // docs/API_VERIFICATION.md §2.2 实测可用的组合
        assert_eq!(
            intents::GUILDS
                | intents::GUILD_MESSAGES
                | intents::GUILD_MESSAGE_REACTIONS
                | intents::DIRECT_MESSAGE
                | intents::GROUP_AND_C2C_EVENT
                | intents::INTERACTION,
            100668929
        );
        assert_eq!(intents::ALL, 2113934851);
    }
}
