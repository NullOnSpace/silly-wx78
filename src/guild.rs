//! 频道（Guild）相关 REST 接口封装。
//!
//! 所有接口行为均以 [`docs/API_VERIFICATION.md`] 的真实请求验证结论为准，重点包括：
//!
//! - **空响应体**：大量删除类操作返回 `HTTP 204` / 空 body / `{}`，解析必须容忍；
//! - **主动消息审核**：无 `msg_id` 的主动消息可能返回 HTTP 200 但 body 携带
//!   `code=304023` + `audit_id`，表示进入安全审核而非失败，见 [`ChannelMessageSendOutcome`]；
//! - **指定消息包装**：`GET /channels/{cid}/messages/{mid}` 响应有外层包装 `{ "message": {...} }`，
//!   且返回的 id 是更长的内部 id，后续操作仍应使用原 id；
//! - **日程列表空值**：`GET /channels/{cid}/schedules` 无数据时返回字面量 `null`；
//! - **创建身份组**：响应字段是 `role_id`（`role` 为 null），不要取 `id`；
//! - **ID 与权限位图均为字符串**，权限位图是十进制字符串（`1`=可查看 `2`=可管理 `4`=可发言）。
//!
//! # 平台侧频控与时限（供业务设计参考）
//! - 被动回复 `msg_id` 有效期约 5 分钟，同一 `msg_id` 最多回复 5 次，需用 `msg_seq` 去重；
//! - 主动消息需审核通道且占用月度配额，不建议作为主流程依赖；
//! - 撤回窗口 2 分钟（仅自己的消息；管理员可撤回成员消息）；
//! - 精华消息每子频道上限 20 条。
//!
//! [`docs/API_VERIFICATION.md`]: ../docs/API_VERIFICATION.md

use crate::auth::AuthClient;
use crate::de;
use crate::{API_BASE_URL, ApiError, parse_empty_response, parse_response};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, error, info, warn};

#[derive(Error, Debug)]
pub enum GuildError {
    #[error("HTTP请求错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("认证错误: {0}")]
    Auth(#[from] crate::auth::AuthError),

    #[error("API错误: {0}")]
    Api(String),

    #[error("JSON解析错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("无效请求: {0}")]
    InvalidRequest(String),
}

impl ApiError for GuildError {
    fn from_api(msg: String) -> Self {
        Self::Api(msg)
    }

    fn from_json(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

// ---------------------------------------------------------------------------
// 基础数据结构
// ---------------------------------------------------------------------------

/// 平台用户信息（机器人自身信息、成员的 user 字段共用此结构）。
///
/// `GET /users/@me` 实测额外返回 `share_url`；成员场景下其余字段可能缺省。
#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub username: Option<String>,
    pub avatar: Option<String>,
    #[serde(default, deserialize_with = "de::opt_lenient_bool")]
    pub bot: Option<bool>,
    pub union_openid: Option<String>,
    /// 仅 `GET /users/@me` 返回
    pub share_url: Option<String>,
}

/// 频道（服务器/ guild）信息。
#[derive(Debug, Clone, Deserialize)]
pub struct Guild {
    pub id: String,
    pub name: Option<String>,
    pub icon: Option<String>,
    pub description: Option<String>,
    pub owner_id: Option<String>,
    #[serde(default, deserialize_with = "de::opt_lenient_bool")]
    pub owner: Option<bool>,
    #[serde(default, deserialize_with = "de::opt_lenient_i64")]
    pub member_count: Option<i64>,
    #[serde(default, deserialize_with = "de::opt_lenient_i64")]
    pub max_members: Option<i64>,
    pub joined_at: Option<String>,
    /// 仅推送事件（GUILD_CREATE/GUILD_UPDATE/GUILD_DELETE）中出现的操作者 ID，
    /// REST 接口不返回该字段。
    #[serde(default)]
    pub op_user_id: Option<String>,
}

/// 子频道类型枚举（依据 docs/API_VERIFICATION.md §4.3 实测值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// 文字子频道，可收发消息
    Text,
    /// 直播子频道（官方文档标注为 1，本次验证未出现）
    Live,
    /// 语音子频道（实测 type=2），online_nums 可用
    Voice,
    /// 分组/分类（type=4），不是真实子频道，对其调接口会报「频道不存在」
    Category,
    /// 帖子/论坛区（type=10007），不产生文字消息事件
    Post,
    /// 日程子频道（type=10011），schedules 接口的目标
    Schedule,
    Unknown(i32),
}

impl From<i32> for ChannelKind {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Text,
            1 => Self::Live,
            2 => Self::Voice,
            4 => Self::Category,
            10007 => Self::Post,
            10011 => Self::Schedule,
            other => Self::Unknown(other),
        }
    }
}

/// 子频道信息。注意 `CHANNEL_UPDATE` 推送比 REST 多出 `op_user_id` 字段。
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub guild_id: Option<String>,
    pub name: Option<String>,
    #[serde(default, rename = "type")]
    raw_type: i32,
    #[serde(default, rename = "sub_type")]
    raw_sub_type: i32,
    pub position: Option<i32>,
    pub parent_id: Option<String>,
    pub owner_id: Option<String>,
    pub private_type: Option<i32>,
    pub speak_permission: Option<i32>,
    pub application_id: Option<String>,
    pub permissions: Option<String>,
    #[serde(default)]
    pub op_user_id: Option<String>,
}

impl ChannelInfo {
    /// 平台原始 type 值
    pub fn raw_type(&self) -> i32 {
        self.raw_type
    }

    /// 子频道 sub_type 值
    pub fn raw_sub_type(&self) -> i32 {
        self.raw_sub_type
    }

    /// 解析后的子频道类型
    pub fn kind(&self) -> ChannelKind {
        ChannelKind::from(self.raw_type)
    }

    /// 是否文字子频道（可收发消息）
    pub fn is_text(&self) -> bool {
        self.kind() == ChannelKind::Text
    }

    /// 是否语音/直播子频道（online_nums 的适用范围）
    pub fn is_voice_or_live(&self) -> bool {
        matches!(self.kind(), ChannelKind::Voice | ChannelKind::Live)
    }

    /// 是否分组(category)。分组不是真实子频道，调多数接口都会报「频道不存在」(11263/40022001)
    pub fn is_category(&self) -> bool {
        self.kind() == ChannelKind::Category
    }
}

/// 成员的用户信息与身份组归属。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Member {
    pub user: Option<UserInfo>,
    pub nick: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    pub joined_at: Option<String>,
}

impl Member {
    /// 成员昵称：优先 nick，其次 username，最后回退到 user.id
    pub fn display_name(&self) -> &str {
        if let Some(nick) = self.nick.as_deref().filter(|s| !s.is_empty()) {
            return nick;
        }
        if let Some(user) = &self.user
            && let Some(name) = user.username.as_deref()
        {
            return name;
        }
        self.user.as_ref().map(|u| u.id.as_str()).unwrap_or("")
    }
}

/// 身份组信息。
#[derive(Debug, Clone, Deserialize)]
pub struct Role {
    pub id: String,
    pub name: Option<String>,
    pub color: Option<u32>,
    // 实测 hoist 返回整数 1/0 而非布尔值
    #[serde(default, deserialize_with = "de::opt_lenient_bool")]
    pub hoist: Option<bool>,
    #[serde(default, deserialize_with = "de::opt_lenient_i64")]
    pub number: Option<i64>,
    #[serde(default, deserialize_with = "de::opt_lenient_i64")]
    pub member_limit: Option<i64>,
}

/// 默认身份组 ID（实测，见 API_VERIFICATION.md §4.5）：
/// `1`=普通成员(全体)，`2`=超级管理员，`4`=频道主，`5`=子频道管理员，`6`=访客。
pub mod default_role_ids {
    pub const EVERYONE: &str = "1";
    pub const ADMIN: &str = "2";
    pub const OWNER: &str = "4";
    pub const CHANNEL_ADMIN: &str = "5";
    pub const GUEST: &str = "6";
}

/// `GET /guilds/{gid}/roles` 响应：`{guild_id, roles[], role_num_limit:"100"}`
#[derive(Debug, Clone, Deserialize)]
pub struct GuildRoles {
    pub guild_id: Option<String>,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(alias = "number_limit", default)]
    pub role_num_limit: Option<String>,
}

/// 创建/修改身份组的响应。⚠️ 实测字段是 `role_id`（`role` 为 null），不要取 `id`。
#[derive(Debug, Clone, Deserialize)]
pub struct RoleCreated {
    pub role_id: String,
    pub role: Option<Role>,
}

#[derive(Debug, Serialize)]
pub struct CreateRoleRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hoist: Option<bool>,
}

impl CreateRoleRequest {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            color: None,
            hoist: None,
        }
    }

    pub fn with_color(mut self, color: u32) -> Self {
        self.color = Some(color);
        self
    }

    pub fn with_hoist(mut self, hoist: bool) -> Self {
        self.hoist = Some(hoist);
        self
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ModifyRoleRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hoist: Option<bool>,
}

/// 子频道权限响应。permissions 是十进制字符串位图：`1`=可查看，`2`=可管理，`4`=可发言。
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelPermissions {
    #[serde(default)]
    pub permissions: String,
}

impl ChannelPermissions {
    /// 解析位图（空串/非法值按 0 处理）
    pub fn bits(&self) -> u64 {
        self.permissions.trim().parse().unwrap_or(0)
    }

    pub fn can_view(&self) -> bool {
        self.bits() & 1 != 0
    }

    pub fn can_manage(&self) -> bool {
        self.bits() & 2 != 0
    }

    pub fn can_speak(&self) -> bool {
        self.bits() & 4 != 0
    }
}

/// `GET /guilds/{gid}/api_permission` 单条记录
#[derive(Debug, Clone, Deserialize)]
pub struct ApiPermissionEntry {
    pub path: Option<String>,
    pub method: Option<String>,
    #[serde(default)]
    pub desc: Option<String>,
    /// 1 = 已授权
    pub auth_status: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiPermissionList {
    #[serde(default)]
    pub apis: Vec<ApiPermissionEntry>,
}

/// 频道消息频率设置。
///
/// 该接口的字段集合未在验证中完整确认，已知布尔开关之外的内容统一落入
/// [`MessageSetting::extra`]，避免静默丢数据。
#[derive(Debug, Clone, Deserialize)]
pub struct MessageSetting {
    #[serde(default, deserialize_with = "de::lenient_bool")]
    pub disable_push_msg: bool,
    #[serde(default, deserialize_with = "de::lenient_bool")]
    pub disable_create_msg: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 修改子频道请求（PATCH /channels/{cid}）。所有字段可选，至少设置一项。
#[derive(Debug, Default, Clone, Serialize)]
pub struct PatchChannelRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_type: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speak_permission: Option<i32>,
}

impl PatchChannelRequest {
    pub fn new() -> Self {
        Self::default()
    }

    /// 改名（实测闭环通过）
    pub fn rename(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn set_position(mut self, position: i32) -> Self {
        self.position = Some(position);
        self
    }

    pub fn move_to_parent(mut self, parent_id: impl Into<String>) -> Self {
        self.parent_id = Some(parent_id.into());
        self
    }

    pub fn set_private_type(mut self, private_type: i32) -> Self {
        self.private_type = Some(private_type);
        self
    }

    pub fn set_speak_permission(mut self, speak_permission: i32) -> Self {
        self.speak_permission = Some(speak_permission);
        self
    }

    fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.position.is_none()
            && self.parent_id.is_none()
            && self.private_type.is_none()
            && self.speak_permission.is_none()
    }
}

// ---------------------------------------------------------------------------
// 频道消息
// ---------------------------------------------------------------------------

/// 频道消息对象（发送成功响应、精华消息列表、指定消息查询均使用此结构）。
///
/// ⚠️ `GET .../messages/{mid}` 返回的 id 是更长的内部 id，后续操作应继续使用原 id。
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelMessage {
    pub id: String,
    pub channel_id: Option<String>,
    pub guild_id: Option<String>,
    pub content: Option<String>,
    pub timestamp: Option<String>,
    pub author: Option<UserInfo>,
    pub member: Option<Member>,
}

/// 发送频道消息请求体。
///
/// - 带 `msg_id` 即被动回复（有效期 5 分钟、最多回复 5 次，用 `msg_seq` 区分多次回复）；
/// - 不带 `msg_id` 即主动消息，会进入安全审核通道并消耗月度配额，第一版不建议依赖。
#[derive(Debug, Default, Clone, Serialize)]
pub struct ChannelMessageRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_seq: Option<i32>,
}

impl ChannelMessageRequest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            ..Self::default()
        }
    }

    /// 构造被动回复（携带 msg_id 与 msg_seq）
    pub fn reply(msg_id: impl Into<String>, content: impl Into<String>, msg_seq: i32) -> Self {
        Self {
            content: Some(content.into()),
            msg_id: Some(msg_id.into()),
            msg_seq: Some(msg_seq),
            ..Self::default()
        }
    }

    pub fn with_image(mut self, url: impl Into<String>) -> Self {
        self.image = Some(url.into());
        self
    }

    /// 设置 msg_seq（同一 msg_id 的第 n 次回复，从 1 开始）
    pub fn with_msg_seq(mut self, msg_seq: i32) -> Self {
        self.msg_seq = Some(msg_seq);
        self
    }
}

/// 主动消息安全审核信息（HTTP 200 但未即时送达）。
#[derive(Debug, Clone, Deserialize)]
pub struct MessageAuditInfo {
    #[serde(default, deserialize_with = "de::opt_lenient_i64")]
    pub code: Option<i64>,
    pub message: Option<String>,
    pub audit_id: Option<String>,
}

/// 发送频道消息的结果。
///
/// 实测：被动回复即时送达；主动消息可能 HTTP 200 但进入安全审核
/// （body 含 `code=304023` + `audit_id`），需订阅 MESSAGE_AUDIT(1<<27) 才能拿到最终结果。
#[derive(Debug, Clone)]
pub enum ChannelMessageSendOutcome {
    /// 正常创建成功，返回完整消息对象
    Created(Box<ChannelMessage>),
    /// 进入安全审核，不会即时送达
    Auditing(MessageAuditInfo),
    /// 其他无法识别的 2xx 响应体，保留原始 JSON 以便排查
    Other(serde_json::Value),
}

impl ChannelMessageSendOutcome {
    /// 若创建成功则返回消息引用
    pub fn created(&self) -> Option<&ChannelMessage> {
        match self {
            Self::Created(m) => Some(m),
            _ => None,
        }
    }

    /// 若进入安全审核则返回审核信息
    pub fn auditing(&self) -> Option<&MessageAuditInfo> {
        match self {
            Self::Auditing(a) => Some(a),
            _ => None,
        }
    }
}

/// 对发送响应体的分类（纯函数，便于单测）。
fn classify_send_body(text: &str) -> Result<ChannelMessageSendOutcome, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    match &value {
        serde_json::Value::Object(map) if map.contains_key("audit_id") => {
            let info: MessageAuditInfo = serde_json::from_value(value.clone())?;
            warn!(
                audit_id = ?info.audit_id,
                code = ?info.code,
                "主动消息进入安全审核，未即时送达"
            );
            Ok(ChannelMessageSendOutcome::Auditing(info))
        }
        serde_json::Value::Array(items) => match items.first() {
            Some(first) => Ok(ChannelMessageSendOutcome::Created(Box::new(
                serde_json::from_value::<ChannelMessage>(first.clone())?,
            ))),
            None => Ok(ChannelMessageSendOutcome::Other(value)),
        },
        _ => match serde_json::from_value::<ChannelMessage>(value.clone()) {
            Ok(message) => Ok(ChannelMessageSendOutcome::Created(Box::new(message))),
            Err(_) => Ok(ChannelMessageSendOutcome::Other(value)),
        },
    }
}

/// `GET /channels/{cid}/messages/{mid}` 的外层包装
#[derive(Debug, Deserialize)]
struct MessageWrapper {
    message: ChannelMessage,
}

// ---------------------------------------------------------------------------
// 精华消息 / 表情表态
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PinMessages {
    #[serde(default)]
    pub messages: Vec<ChannelMessage>,
}

/// 表情表态类型：`0`=系统表情，`1`=emoji 表情
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum ReactionType {
    SystemFace = 0,
    Emoji = 1,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReactionUser {
    pub id: String,
    pub username: Option<String>,
    pub avatar: Option<String>,
}

/// 表情表态用户列表页（cookie 游标分页）
#[derive(Debug, Clone, Deserialize)]
pub struct ReactionUsersPage {
    #[serde(default)]
    pub data: Vec<ReactionUser>,
    #[serde(default)]
    pub cookie: Option<String>,
    #[serde(default, deserialize_with = "de::opt_lenient_bool")]
    pub is_end: Option<bool>,
}

impl ReactionUsersPage {
    pub fn is_end(&self) -> bool {
        self.is_end.unwrap_or(true)
    }
}

// ---------------------------------------------------------------------------
// 日程
// ---------------------------------------------------------------------------

/// 日程提醒类型（实测字段名是 `remind_type`，不是 reminder_type）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemindType {
    /// 不提醒
    #[serde(rename = "0")]
    None,
    /// 开始时提醒
    #[serde(rename = "1")]
    AtStart,
    /// 开始前 5 分钟
    #[serde(rename = "2")]
    FiveMinutesBefore,
    /// 开始前 15 分钟
    #[serde(rename = "3")]
    FifteenMinutesBefore,
    /// 开始前 30 分钟
    #[serde(rename = "4")]
    ThirtyMinutesBefore,
    /// 开始前 60 分钟
    #[serde(rename = "5")]
    OneHourBefore,
}

/// 日程信息。时间戳是**毫秒**字符串（实测），反序列化已做宽松处理。
#[derive(Debug, Clone, Deserialize)]
pub struct Schedule {
    pub id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(default, deserialize_with = "de::opt_lenient_i64")]
    pub start_timestamp: Option<i64>,
    #[serde(default, deserialize_with = "de::opt_lenient_i64")]
    pub end_timestamp: Option<i64>,
    pub jump_channel_id: Option<String>,
    pub remind_type: Option<RemindType>,
    pub creator: Option<Member>,
}

mod ser {
    use serde::Serializer;

    /// i64 序列化为字符串（平台要求毫秒时间戳以字符串传输）
    pub fn i64_as_string<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
}

/// 创建日程请求（POST 时包一层 `{"schedule": {...}}` 信封）。
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 开始时间（毫秒时间戳，序列化为字符串）
    #[serde(serialize_with = "ser::i64_as_string")]
    pub start_timestamp: i64,
    /// 结束时间（毫秒时间戳，序列化为字符串）
    #[serde(serialize_with = "ser::i64_as_string")]
    pub end_timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jump_channel_id: Option<String>,
    pub remind_type: RemindType,
}

impl ScheduleRequest {
    pub fn new(name: impl Into<String>, start_timestamp_ms: i64, end_timestamp_ms: i64) -> Self {
        Self {
            name: name.into(),
            description: None,
            start_timestamp: start_timestamp_ms,
            end_timestamp: end_timestamp_ms,
            jump_channel_id: None,
            remind_type: RemindType::AtStart,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_jump_channel(mut self, channel_id: impl Into<String>) -> Self {
        self.jump_channel_id = Some(channel_id.into());
        self
    }

    pub fn with_remind_type(mut self, remind_type: RemindType) -> Self {
        self.remind_type = remind_type;
        self
    }

    fn envelope(schedule: &ScheduleRequest) -> impl Serialize + use<'_> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            schedule: &'a ScheduleRequest,
        }
        Envelope { schedule }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OnlineNums {
    #[serde(default, deserialize_with = "de::lenient_i64")]
    pub online_nums: i64,
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

const DEFAULT_PAGE_LIMIT: u16 = 50;

/// 频道相关 REST 接口客户端。
pub struct GuildClient {
    http_client: Arc<Client>,
    auth_client: Arc<AuthClient>,
}

impl GuildClient {
    pub fn new(http_client: Arc<Client>, auth_client: Arc<AuthClient>) -> Self {
        Self {
            http_client,
            auth_client,
        }
    }

    async fn send_request(
        &self,
        method: Method,
        url: &str,
    ) -> Result<reqwest::Response, GuildError> {
        let authorization = self.auth_client.get_authorization().await?;
        debug!(method = %method, url = %url, "发送请求");
        Ok(self
            .http_client
            .request(method, url)
            .header("Authorization", &authorization)
            .send()
            .await?)
    }

    async fn send_json_request<T: Serialize>(
        &self,
        method: Method,
        url: &str,
        body: &T,
    ) -> Result<reqwest::Response, GuildError> {
        let authorization = self.auth_client.get_authorization().await?;
        debug!(method = %method, url = %url, "发送带JSON请求");
        Ok(self
            .http_client
            .request(method, url)
            .header("Authorization", &authorization)
            .json(body)
            .send()
            .await?)
    }

    // -- 第一档：基础信息 ---------------------------------------------------

    /// 机器人自身信息（GET /users/@me，含 share_url）
    pub async fn get_me(&self) -> Result<UserInfo, GuildError> {
        let url = format!("{API_BASE_URL}/users/@me");
        let response = self.send_request(Method::GET, &url).await?;
        let me: UserInfo = parse_response::<_, GuildError>(response).await?;
        info!(user_id = %me.id, "获取机器人信息成功");
        Ok(me)
    }

    /// 机器人所在的频道列表（GET /users/@me/guilds）
    pub async fn get_guilds(&self, limit: Option<u16>) -> Result<Vec<Guild>, GuildError> {
        let url = format!("{API_BASE_URL}/users/@me/guilds");
        let response = self.http_client.get(&url);
        let response = match limit {
            Some(limit) => response.query(&[("limit", limit)]),
            None => response,
        };
        let authorization = self.auth_client.get_authorization().await?;
        let response = response
            .header("Authorization", &authorization)
            .send()
            .await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 频道详情（GET /guilds/{gid}）
    pub async fn get_guild(&self, guild_id: &str) -> Result<Guild, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}");
        let response = self.send_request(Method::GET, &url).await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 子频道列表（GET /guilds/{gid}/channels）。
    ///
    /// 注意过滤 `kind()==Category` 的分组项——分组不是真实子频道，
    /// 对其调用消息等接口会报「频道不存在」(11263/40022001)。
    pub async fn get_channels(&self, guild_id: &str) -> Result<Vec<ChannelInfo>, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/channels");
        let response = self.send_request(Method::GET, &url).await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 子频道详情（GET /channels/{cid}）
    pub async fn get_channel(&self, channel_id: &str) -> Result<ChannelInfo, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}");
        let response = self.send_request(Method::GET, &url).await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 修改子频道（PATCH /channels/{cid}，实测改名→恢复闭环通过），返回更新后的对象
    pub async fn patch_channel(
        &self,
        channel_id: &str,
        request: PatchChannelRequest,
    ) -> Result<ChannelInfo, GuildError> {
        if request.is_empty() {
            return Err(GuildError::InvalidRequest(
                "PATCH 子频道请求不能为空，至少设置一项字段".to_string(),
            ));
        }
        let url = format!("{API_BASE_URL}/channels/{channel_id}");
        let response = self
            .send_json_request(Method::PATCH, &url, &request)
            .await?;
        info!(channel_id = %channel_id, "修改子频道成功");
        parse_response::<_, GuildError>(response).await
    }

    // -- 第一档：成员与身份组 -------------------------------------------------

    /// 成员列表分页（GET /guilds/{gid}/members?limit=&offset=）。
    /// 返回数量少于 limit 表示已到末尾。
    pub async fn get_members(
        &self,
        guild_id: &str,
        limit: Option<u16>,
        offset: Option<u64>,
    ) -> Result<Vec<Member>, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/members");
        let mut query = Vec::new();
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        if let Some(offset) = offset {
            query.push(("offset", offset.to_string()));
        }
        let authorization = self.auth_client.get_authorization().await?;
        let response = self
            .http_client
            .get(&url)
            .header("Authorization", &authorization)
            .query(&query)
            .send()
            .await?;
        parse_nullable_list_from_response(response).await
    }

    /// 全量拉取成员（自动翻页）。page_size 上限一般为 50，max_pages 用于防御死循环。
    pub async fn get_members_all(
        &self,
        guild_id: &str,
        page_size: u16,
        max_pages: u32,
    ) -> Result<Vec<Member>, GuildError> {
        let page_size = page_size.clamp(1, DEFAULT_PAGE_LIMIT);
        let mut members = Vec::new();
        let mut offset: u64 = 0;
        for _ in 0..max_pages.max(1) {
            let page = self
                .get_members(guild_id, Some(page_size), Some(offset))
                .await?;
            let fetched = page.len();
            members.extend(page);
            if (fetched as u64) < page_size as u64 {
                return Ok(members);
            }
            offset += fetched as u64;
        }
        warn!(guild_id = %guild_id, pages = max_pages, "成员拉取达到最大页数上限，可能不完整");
        Ok(members)
    }

    /// 成员详情（GET /guilds/{gid}/members/{uid}）
    pub async fn get_member(&self, guild_id: &str, user_id: &str) -> Result<Member, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/members/{user_id}");
        let response = self.send_request(Method::GET, &url).await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 身份组列表（GET /guilds/{gid}/roles）
    pub async fn get_roles(&self, guild_id: &str) -> Result<GuildRoles, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/roles");
        let response = self.send_request(Method::GET, &url).await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 创建身份组（POST /guilds/{gid}/roles）。
    ///
    /// ⚠️ 实测响应是 `{role_id:"...", role:null}`，请取 [`RoleCreated::role_id`]。
    pub async fn create_role(
        &self,
        guild_id: &str,
        request: CreateRoleRequest,
    ) -> Result<RoleCreated, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/roles");
        let response = self.send_json_request(Method::POST, &url, &request).await?;
        let role: RoleCreated = parse_response::<_, GuildError>(response).await?;
        info!(guild_id = %guild_id, role_id = %role.role_id, "创建身份组成功");
        Ok(role)
    }

    /// 修改身份组（PATCH /guilds/{gid}/roles/{rid}）。响应结构与创建相同（role_id 字段）。
    pub async fn modify_role(
        &self,
        guild_id: &str,
        role_id: &str,
        request: ModifyRoleRequest,
    ) -> Result<RoleCreated, GuildError> {
        if request.name.is_none() && request.color.is_none() && request.hoist.is_none() {
            return Err(GuildError::InvalidRequest(
                "修改身份组请求不能为空".to_string(),
            ));
        }
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/roles/{role_id}");
        let response = self
            .send_json_request(Method::PATCH, &url, &request)
            .await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 删除身份组（DELETE /guilds/{gid}/roles/{rid}，实测返回 204 空体）
    pub async fn delete_role(&self, guild_id: &str, role_id: &str) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/roles/{role_id}");
        let response = self.send_request(Method::DELETE, &url).await?;
        parse_empty_response::<GuildError>(response).await
    }

    /// 身份组成员分页（GET /guilds/{gid}/roles/{rid}/members）。
    /// `next` 游标来自上一页响应，"0" 表示结束。
    pub async fn get_role_members_page(
        &self,
        guild_id: &str,
        role_id: &str,
        next: Option<&str>,
        limit: Option<u16>,
    ) -> Result<RoleMembersPage, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/roles/{role_id}/members");
        let mut query = Vec::new();
        if let Some(next) = next {
            query.push(("next", next.to_string()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let authorization = self.auth_client.get_authorization().await?;
        let response = self
            .http_client
            .get(&url)
            .header("Authorization", &authorization)
            .query(&query)
            .send()
            .await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 全量拉取某身份组成员（自动翻页，max_pages 防御异常游标导致的死循环）
    pub async fn get_role_members_all(
        &self,
        guild_id: &str,
        role_id: &str,
        max_pages: u32,
    ) -> Result<Vec<Member>, GuildError> {
        let mut members = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..max_pages.max(1) {
            let page = self
                .get_role_members_page(guild_id, role_id, cursor.as_deref(), None)
                .await?;
            for member in &page.data {
                if let Some(user) = &member.user
                    && seen.insert(user.id.clone())
                {
                    members.push(member.clone());
                }
            }
            match page.next.as_deref() {
                Some(next) if next != "0" && !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(members),
            }
        }
        warn!(guild_id = %guild_id, role_id = %role_id, "身份组成员拉取达到最大页数上限");
        Ok(members)
    }

    // -- 第一档：权限与配置 ---------------------------------------------------

    /// 子频道指定用户的权限（GET /channels/{cid}/members/{uid}/permissions）
    pub async fn get_channel_user_permissions(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<ChannelPermissions, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/members/{user_id}/permissions");
        let response = self.send_request(Method::GET, &url).await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 子频道指定身份组的权限（GET /channels/{cid}/roles/{rid}/permissions）
    pub async fn get_channel_role_permissions(
        &self,
        channel_id: &str,
        role_id: &str,
    ) -> Result<ChannelPermissions, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/roles/{role_id}/permissions");
        let response = self.send_request(Method::GET, &url).await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 机器人在频道的 API 权限（GET /guilds/{gid}/api_permission）
    pub async fn get_api_permission(
        &self,
        guild_id: &str,
    ) -> Result<ApiPermissionList, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/api_permission");
        let response = self.send_request(Method::GET, &url).await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 频道消息频率设置（GET /guilds/{gid}/message/setting）
    pub async fn get_message_setting(&self, guild_id: &str) -> Result<MessageSetting, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/message/setting");
        let response = self.send_request(Method::GET, &url).await?;
        parse_response::<_, GuildError>(response).await
    }

    // -- 第一档：消息 ---------------------------------------------------------

    /// 发送频道消息（POST /channels/{cid}/messages）。
    ///
    /// - 被动回复（带 msg_id）：即时送达，msg_id 有效期 5 分钟，同一 msg_id 最多回复 5 次；
    /// - 主动消息（无 msg_id）：可能进入安全审核，用 [`ChannelMessageSendOutcome`] 判断。
    pub async fn send_channel_message(
        &self,
        channel_id: &str,
        request: ChannelMessageRequest,
    ) -> Result<ChannelMessageSendOutcome, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/messages");
        let response = self.send_json_request(Method::POST, &url, &request).await?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            error!(status = %status, body = %text, "发送频道消息失败");
            return Err(GuildError::Api(text));
        }

        debug!(status = %status, body = %text, "发送频道消息响应");
        classify_send_body(&text).map_err(GuildError::Json)
    }

    /// 发送主动文本消息（无 msg_id，可能进入安全审核，消耗月度配额）
    pub async fn send_text(
        &self,
        channel_id: &str,
        content: impl Into<String>,
    ) -> Result<ChannelMessageSendOutcome, GuildError> {
        let request = ChannelMessageRequest::text(content);
        self.send_channel_message(channel_id, request).await
    }

    /// 被动回复频道消息。msg_seq 从 1 开始，用于同一 msg_id 的多次回复去重。
    pub async fn reply_text(
        &self,
        channel_id: &str,
        msg_id: impl Into<String>,
        content: impl Into<String>,
        msg_seq: i32,
    ) -> Result<ChannelMessageSendOutcome, GuildError> {
        let request = ChannelMessageRequest::reply(msg_id, content, msg_seq);
        self.send_channel_message(channel_id, request).await
    }

    /// 获取指定消息（GET /channels/{cid}/messages/{mid}）。
    ///
    /// ⚠️ 实测响应有外层包装 `{message:{...}}`，且返回的 id 是更长的内部 id；
    /// 后续撤回/表态等操作仍应使用原始事件中的 id。
    pub async fn get_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<ChannelMessage, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/messages/{message_id}");
        let response = self.send_request(Method::GET, &url).await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            error!(status = %status, body = %text, "获取指定消息失败");
            return Err(GuildError::Api(text));
        }
        // 兼容包装与非包装两种形态
        match serde_json::from_str::<MessageWrapper>(&text) {
            Ok(wrapper) => Ok(wrapper.message),
            Err(_) => Ok(serde_json::from_str::<ChannelMessage>(&text)?),
        }
    }

    /// 撤回消息（DELETE /channels/{cid}/messages/{mid}）。
    ///
    /// 撤回自己的消息窗口为 2 分钟；管理员可撤回成员消息。
    /// hide_tip=true 时提示「对所有人隐藏该消息」的提示条。
    pub async fn recall_message(
        &self,
        channel_id: &str,
        message_id: &str,
        hide_tip: bool,
    ) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/messages/{message_id}");
        let authorization = self.auth_client.get_authorization().await?;
        let response = self
            .http_client
            .delete(&url)
            .header("Authorization", &authorization)
            .query(&[("hidetip", hide_tip.to_string())])
            .send()
            .await?;
        info!(channel_id = %channel_id, message_id = %message_id, "撤回消息成功");
        parse_empty_response::<GuildError>(response).await
    }

    // -- 第二档：精华消息 -------------------------------------------------------

    /// 获取精华消息列表（GET /channels/{cid}/pins），每子频道上限 20 条
    pub async fn get_pins(&self, channel_id: &str) -> Result<Vec<ChannelMessage>, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/pins");
        let response = self.send_request(Method::GET, &url).await?;
        let text = read_body_text(response).await;
        if is_nullish(&text) {
            return Ok(Vec::new());
        }
        let pins: PinMessages = serde_json::from_str(&text)?;
        Ok(pins.messages)
    }

    /// 添加精华消息（PUT /channels/{cid}/pins/{mid}，204 空体）
    pub async fn add_pin(&self, channel_id: &str, message_id: &str) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/pins/{message_id}");
        let response = self.send_request(Method::PUT, &url).await?;
        info!(channel_id = %channel_id, message_id = %message_id, "添加精华消息成功");
        parse_empty_response::<GuildError>(response).await
    }

    /// 移除精华消息（DELETE /channels/{cid}/pins/{mid}，204 空体）
    pub async fn remove_pin(&self, channel_id: &str, message_id: &str) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/pins/{message_id}");
        let response = self.send_request(Method::DELETE, &url).await?;
        info!(channel_id = %channel_id, message_id = %message_id, "移除精华消息成功");
        parse_empty_response::<GuildError>(response).await
    }

    // -- 第二档：表情表态 -------------------------------------------------------

    /// 对消息添加表情表态（PUT .../reactions/{type}/{id}，204 空体）。
    ///
    /// ⚠️ 重复添加同一表态平台会报 500 `620003 already had reaction`，业务侧建议幂等处理或先删除。
    pub async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        reaction_type: ReactionType,
        emoji_id: &str,
    ) -> Result<(), GuildError> {
        let url = reaction_url(channel_id, message_id, reaction_type, emoji_id);
        let response = self.send_request(Method::PUT, &url).await?;
        parse_empty_response::<GuildError>(response).await
    }

    /// 删除自己对消息的表情表态（DELETE .../reactions/{type}/{id}，204 空体）
    pub async fn delete_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        reaction_type: ReactionType,
        emoji_id: &str,
    ) -> Result<(), GuildError> {
        let url = reaction_url(channel_id, message_id, reaction_type, emoji_id);
        let response = self.send_request(Method::DELETE, &url).await?;
        parse_empty_response::<GuildError>(response).await
    }

    /// 表情表态用户列表（GET .../reactions/{type}/{id}?cookie=&limit=，cookie 游标分页）
    pub async fn get_reaction_users(
        &self,
        channel_id: &str,
        message_id: &str,
        reaction_type: ReactionType,
        emoji_id: &str,
        cookie: Option<&str>,
        limit: Option<u16>,
    ) -> Result<ReactionUsersPage, GuildError> {
        let url = reaction_url(channel_id, message_id, reaction_type, emoji_id);
        let mut query = Vec::new();
        if let Some(cookie) = cookie {
            query.push(("cookie", cookie.to_string()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let authorization = self.auth_client.get_authorization().await?;
        let response = self
            .http_client
            .get(&url)
            .header("Authorization", &authorization)
            .query(&query)
            .send()
            .await?;
        let text = read_body_text(response).await;
        if is_nullish(&text) {
            return Ok(ReactionUsersPage {
                data: Vec::new(),
                cookie: None,
                is_end: Some(true),
            });
        }
        Ok(serde_json::from_str(&text)?)
    }

    // -- 第二档：日程 ------------------------------------------------------------

    /// 日程列表（GET /channels/{cid}/schedules?since=毫秒时间戳）。
    ///
    /// ⚠️ 实测空列表时平台返回字面量 `null`，本方法已容忍并返回空 Vec。
    pub async fn get_schedules(
        &self,
        channel_id: &str,
        since_ms: Option<i64>,
    ) -> Result<Vec<Schedule>, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/schedules");
        let mut request = {
            let authorization = self.auth_client.get_authorization().await?;
            self.http_client
                .get(&url)
                .header("Authorization", &authorization)
        };
        if let Some(since_ms) = since_ms {
            request = request.query(&[("since", since_ms.to_string())]);
        }
        let response = request.send().await?;
        parse_nullable_list_from_response(response).await
    }

    /// 单个日程详情（GET /channels/{cid}/schedules/{sid}）
    pub async fn get_schedule(
        &self,
        channel_id: &str,
        schedule_id: &str,
    ) -> Result<Option<Schedule>, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/schedules/{schedule_id}");
        let response = self.send_request(Method::GET, &url).await?;
        let text = read_body_text(response).await;
        if is_nullish(&text) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&text)?))
    }

    /// 创建日程（POST /channels/{cid}/schedules）。
    ///
    /// ⚠️ 实测要点：字段必须是 `remind_type`（不是 reminder_type）；时间戳是毫秒字符串。
    pub async fn create_schedule(
        &self,
        channel_id: &str,
        request: ScheduleRequest,
    ) -> Result<Option<Schedule>, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/schedules");
        let envelope = ScheduleRequest::envelope(&request);
        let response = self
            .send_json_request(Method::POST, &url, &envelope)
            .await?;
        let text = read_body_text(response).await;
        if is_nullish(&text) {
            warn!("创建日程返回空响应");
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&text)?))
    }

    /// 删除日程（DELETE /channels/{cid}/schedules/{sid}）。
    ///
    /// ⚠️ 验证期间平台对该接口持续返回 `50015001 系统繁忙`（重试无效的平台故障），
    /// 使用时请做好失败重试与兜底。
    pub async fn delete_schedule(
        &self,
        channel_id: &str,
        schedule_id: &str,
    ) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/schedules/{schedule_id}");
        #[derive(Serialize)]
        struct IdEnvelope<'a> {
            schedule: IdField<'a>,
        }
        #[derive(Serialize)]
        struct IdField<'a> {
            id: &'a str,
        }
        let body = IdEnvelope {
            schedule: IdField { id: schedule_id },
        };
        let response = self.send_json_request(Method::DELETE, &url, &body).await?;
        info!(channel_id = %channel_id, schedule_id = %schedule_id, "删除日程成功");
        parse_empty_response::<GuildError>(response).await
    }

    // -- 第二档：在线人数 ---------------------------------------------------------

    /// 子频道在线人数（GET /channels/{cid}/online_nums）。
    ///
    /// ⚠️ 实测仅语音/直播子频道(type=1/2)有效；文字子频道报 505007；
    /// 分组(type=4)报「频道不存在」。建议先用 [`Self::get_channel`] 判断类型再调用。
    pub async fn get_online_nums(&self, channel_id: &str) -> Result<i64, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/online_nums");
        let response = self.send_request(Method::GET, &url).await?;
        let nums: OnlineNums = parse_response::<_, GuildError>(response).await?;
        Ok(nums.online_nums)
    }
}

/// 身份组成员分页响应：`{data:[Member], next:"0"}`（next 游标分页）
#[derive(Debug, Clone, Deserialize)]
pub struct RoleMembersPage {
    #[serde(default)]
    pub data: Vec<Member>,
    #[serde(default)]
    pub next: Option<String>,
}

fn reaction_url(
    channel_id: &str,
    message_id: &str,
    reaction_type: ReactionType,
    emoji_id: &str,
) -> String {
    format!(
        "{API_BASE_URL}/channels/{channel_id}/messages/{message_id}/reactions/{}/{}",
        reaction_type as u8, emoji_id
    )
}

async fn read_body_text(response: reqwest::Response) -> String {
    response.text().await.unwrap_or_default()
}

fn is_nullish(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty() || trimmed == "{}" || trimmed == "null"
}

/// 解析"数组或字面量 null"形态的响应（如日程列表空时返回 null）。
async fn parse_nullable_list_from_response<T, E>(response: reqwest::Response) -> Result<Vec<T>, E>
where
    T: serde::de::DeserializeOwned,
    E: ApiError,
{
    let status = response.status();
    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "未知错误".to_string());
        error!(status = %status, error = %error_text, "API请求失败");
        return Err(E::from_api(error_text));
    }
    let text = read_body_text(response).await;
    debug!(body = %text, "收到数组响应");
    if is_nullish(&text) {
        return Ok(Vec::new());
    }
    serde_json::from_str(&text).map_err(E::from_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bot_user_with_share_url() {
        let body = r#"{
            "id": "13370821155780965225",
            "username": "饥荒联机信息助手",
            "avatar": "https://example.com/a.png",
            "bot": true,
            "share_url": "https://qq.qq.com/#/addBot?appid=1"
        }"#;
        let user: UserInfo = serde_json::from_str(body).unwrap();
        assert_eq!(user.id, "13370821155780965225");
        assert_eq!(
            user.share_url.as_deref(),
            Some("https://qq.qq.com/#/addBot?appid=1")
        );
    }

    #[test]
    fn channel_kind_mapping_matches_verification() {
        assert_eq!(ChannelKind::from(0), ChannelKind::Text);
        assert_eq!(ChannelKind::from(2), ChannelKind::Voice);
        assert_eq!(ChannelKind::from(4), ChannelKind::Category);
        assert_eq!(ChannelKind::from(10007), ChannelKind::Post);
        assert_eq!(ChannelKind::from(10011), ChannelKind::Schedule);
        assert!(matches!(ChannelKind::from(42), ChannelKind::Unknown(42)));
    }

    #[test]
    fn parses_channel_update_payload_with_op_user_id() {
        // docs/API_VERIFICATION.md §4.2 完整样例
        let body = r#"{
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
        let channel: ChannelInfo = serde_json::from_str(body).unwrap();
        assert_eq!(channel.op_user_id.as_deref(), Some("5688363186875882260"));
        assert!(channel.is_text());
        assert!(!channel.is_category());
    }

    #[test]
    fn permission_bits_parse_decimal_strings() {
        let p: ChannelPermissions = serde_json::from_str(r#"{"permissions":"7"}"#).unwrap();
        assert!(p.can_view() && p.can_manage() && p.can_speak());

        let empty: ChannelPermissions = serde_json::from_str(r#"{"permissions":""}"#).unwrap();
        assert_eq!(empty.bits(), 0);
        assert!(!empty.can_view());
    }

    #[test]
    fn role_created_reads_role_id_not_id() {
        // 实测：创建身份组响应是 {role_id:"...", role:null}
        let body = r#"{"role_id":"12345","role":null}"#;
        let created: RoleCreated = serde_json::from_str(body).unwrap();
        assert_eq!(created.role_id, "12345");
        assert!(created.role.is_none());
    }

    #[test]
    fn schedules_null_body_yields_empty_vec() {
        // 实测：日程列表为空时返回字面量 null
        assert!(
            parse_nullable_list_sync::<Schedule>("null")
                .unwrap()
                .is_empty()
        );
        assert!(parse_nullable_list_sync::<Schedule>("").unwrap().is_empty());
        let list =
            parse_nullable_list_sync::<Schedule>(r#"[{"name":"测试","remind_type":"2"}]"#).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].remind_type, Some(RemindType::FiveMinutesBefore));
    }

    fn parse_nullable_list_sync<T: serde::de::DeserializeOwned>(
        text: &str,
    ) -> Result<Vec<T>, serde_json::Error> {
        if is_nullish(text) {
            return Ok(Vec::new());
        }
        serde_json::from_str(text)
    }

    #[test]
    fn schedule_request_wraps_envelope_and_string_timestamps() {
        let request = ScheduleRequest::new("测试日程", 1700000000000, 1700003600000);
        let envelope = ScheduleRequest::envelope(&request);
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            json["schedule"]["start_timestamp"],
            serde_json::Value::String("1700000000000".to_string())
        );
        assert_eq!(json["schedule"]["remind_type"], serde_json::json!("1"));
    }

    #[test]
    fn classify_detects_audit_response() {
        // 实测：主动消息 HTTP 200 但 body 含 code=304023 + audit_id
        let body = r#"{"code":304023,"message":"push audit","audit_id":"abc-123","data":{}}"#;
        let outcome = classify_send_body(body).unwrap();
        let audit = outcome.auditing().expect("应为审核结果");
        assert_eq!(audit.audit_id.as_deref(), Some("abc-123"));
        assert_eq!(audit.code, Some(304023));
    }

    #[test]
    fn classify_created_message_and_array_form() {
        let single = classify_send_body(r#"{"id":"RQm2x","content":"hi"}"#).unwrap();
        assert_eq!(single.created().unwrap().id, "RQm2x");

        let array = classify_send_body(r#"[{"id":"arr-1","content":"hi"}]"#).unwrap();
        assert_eq!(array.created().unwrap().id, "arr-1");

        let weird = classify_send_body(r#"{"foo":1}"#).unwrap();
        assert!(matches!(weird, ChannelMessageSendOutcome::Other(_)));
    }

    #[test]
    fn get_message_unwraps_message_field() {
        // 实测：GET 指定消息响应有外层包装 {message:{...}}
        let wrapped = r#"{"message":{"id":"08bd8ae3...380348b79abbd406","content":"wawa"}}"#;
        let wrapper: MessageWrapper = serde_json::from_str(wrapped).unwrap();
        assert_eq!(wrapper.message.content.as_deref(), Some("wawa"));
    }

    #[test]
    fn lenient_integer_deserializers() {
        #[derive(Deserialize)]
        struct Sample {
            #[serde(default, deserialize_with = "de::lenient_i64")]
            a: i64,
            #[serde(default, deserialize_with = "de::opt_lenient_i64")]
            b: Option<i64>,
        }
        let from_number: Sample = serde_json::from_str(r#"{"a":12,"b":34}"#).unwrap();
        assert_eq!((from_number.a, from_number.b), (12, Some(34)));
        let from_string: Sample = serde_json::from_str(r#"{"a":"56","b":"78"}"#).unwrap();
        assert_eq!((from_string.a, from_string.b), (56, Some(78)));
        let missing: Sample = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!((missing.a, missing.b), (0, None));
    }

    #[test]
    fn role_hoist_accepts_integer_zero_one() {
        // 2026-08-26 真实请求实测：GET /guilds/{gid}/roles 的 hoist 是整数 1/0
        let body = r#"{"guild_id":"17327925269880161597","roles":[
            {"id":"4","name":"频道主","color":4294917938,"hoist":1,"number":1,"member_limit":1},
            {"id":"6","name":"访客","color":4286151052,"hoist":0,"number":0,"member_limit":1000000}
        ],"role_num_limit":"100"}"#;
        let roles: GuildRoles = serde_json::from_str(body).unwrap();
        assert_eq!(roles.roles.len(), 2);
        assert_eq!(roles.roles[0].hoist, Some(true));
        assert_eq!(roles.roles[1].hoist, Some(false));
        assert_eq!(roles.roles[0].color, Some(4294917938));
        assert_eq!(roles.role_num_limit.as_deref(), Some("100"));
    }

    #[test]
    fn lenient_bool_accepts_bool_int_and_string() {
        #[derive(Deserialize)]
        struct Sample {
            #[serde(default, deserialize_with = "de::opt_lenient_bool")]
            a: Option<bool>,
            #[serde(default, deserialize_with = "de::lenient_bool")]
            b: bool,
        }
        let s: Sample = serde_json::from_str(r#"{"a":true,"b":1}"#).unwrap();
        assert_eq!((s.a, s.b), (Some(true), true));
        let s: Sample = serde_json::from_str(r#"{"a":0,"b":"false"}"#).unwrap();
        assert_eq!((s.a, s.b), (Some(false), false));
        let s: Sample = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!((s.a, s.b), (None, false));
    }

    #[test]
    fn member_display_name_fallback_chain() {
        let member: Member = serde_json::from_str(
            r#"{"user":{"id":"u1","username":"besiege"},"nick":"小围","roles":["4"]}"#,
        )
        .unwrap();
        assert_eq!(member.display_name(), "小围");

        let no_nick: Member =
            serde_json::from_str(r#"{"user":{"id":"u1","username":"besiege"}}"#).unwrap();
        assert_eq!(no_nick.display_name(), "besiege");

        let bare: Member = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(bare.display_name(), "");
    }
}
