//! 频道（Guild）相关 REST 接口封装。
//!
//! 接口清单依据 QQ 机器人官方文档 api-v2《频道模块》与《消息收发/频道消息》
//! 实现（频道 CRUD / 成员 / 身份组与权限 / 发言管理 / 内容管理 / 频道消息、
//! 私信、表情表态），行为细节以 [`docs/API_VERIFICATION.md`] 的真实请求验证结论为准，
//! 重点包括：
//!
//! - **空响应体**：大量删除类操作返回 `HTTP 204` / 空 body / `{}`，解析必须容忍；
//! - **主动消息审核**：无 `msg_id`/`event_id` 的主动消息可能返回 HTTP 200 但 body 携带
//!   `code=304023` + `audit_id`（或 `data.message_audit.audit_id`），表示进入安全审核
//!   而非失败，见 [`ChannelMessageSendOutcome`]；
//! - **指定消息包装**：`GET /channels/{cid}/messages/{mid}` 响应有外层包装 `{ "message": {...} }`，
//!   且返回的 id 是更长的内部 id，后续操作仍应使用原 id；
//! - **日程列表空值**：`GET /channels/{cid}/schedules` 无数据时返回字面量 `null`；
//! - **创建身份组**：响应字段是 `role_id`（`role` 为 null），不要取 `id`；
//! - **ID 与权限位图均为字符串**，权限位图是十进制字符串（`1`=可查看 `2`=可管理 `4`=可发言）；
//! - **表情类型**：官方 EmojiType 为 `1`=系统表情、`2`=emoji 表情（旧版 0/1 映射已修正）。
//!
//! # 平台侧频控与时限（供业务设计参考）
//! - 被动回复 `msg_id` 有效期约 5 分钟，同一 `msg_id` 最多回复 5 次，需用 `msg_seq` 去重；
//! - 主动消息需审核通道且占用月度配额，不建议作为主流程依赖；
//! - 撤回窗口 2 分钟（仅自己的消息；管理员可撤回成员消息）；
//! - 精华消息每子频道上限 20 条；
//! - 推荐子频道公告每频道最多 3 条；接口权限授权链接每天每频道默认 3 条。
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

/// 子频道类型枚举（依据官方文档《子频道对象(Channel)》ChannelType）。
///
/// 官方枚举：`0`=文字、`2`=语音、`4`=分组、`10005`=直播、`10006`=应用、
/// `10007`=论坛；`1`/`3` 为保留值不可用。`10011`=日程为实测出现值
/// （见 docs/API_VERIFICATION.md §4.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// 文字子频道，可收发消息
    Text,
    /// 语音子频道（type=2），online_nums 可用
    Voice,
    /// 分组/分类（type=4），不是真实子频道，对其调接口会报「频道不存在」
    Category,
    /// 直播子频道（type=10005），online_nums 可用
    Live,
    /// 应用子频道（type=10006，如王者开黑大厅、日程提醒）
    Application,
    /// 帖子/论坛区（type=10007），不产生文字消息事件
    Post,
    /// 日程子频道（type=10011，实测值），schedules 接口的目标
    Schedule,
    Unknown(i32),
}

impl From<i32> for ChannelKind {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Text,
            2 => Self::Voice,
            4 => Self::Category,
            10005 => Self::Live,
            10006 => Self::Application,
            10007 => Self::Post,
            10011 => Self::Schedule,
            other => Self::Unknown(other),
        }
    }
}

impl ChannelKind {
    /// 平台原始 type 值
    pub fn raw(self) -> i32 {
        match self {
            Self::Text => 0,
            Self::Voice => 2,
            Self::Category => 4,
            Self::Live => 10005,
            Self::Application => 10006,
            Self::Post => 10007,
            Self::Schedule => 10011,
            Self::Unknown(raw) => raw,
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

/// 频道消息频率设置（GET /guilds/{gid}/message/setting）。
///
/// 官方字段：`disable_create_dm`、`disable_push_msg`、`channel_ids`、
/// `channel_push_max_num`；其余未确认字段落入 [`MessageSetting::extra`]，避免静默丢数据。
#[derive(Debug, Clone, Deserialize)]
pub struct MessageSetting {
    #[serde(default, deserialize_with = "de::lenient_bool")]
    pub disable_create_dm: bool,
    #[serde(default, deserialize_with = "de::lenient_bool")]
    pub disable_push_msg: bool,
    #[serde(default)]
    pub channel_ids: Vec<String>,
    #[serde(default, deserialize_with = "de::opt_lenient_i64")]
    pub channel_push_max_num: Option<i64>,
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

/// 创建子频道请求（POST /guilds/{gid}/channels）。
///
/// 官方说明：私域接口，创建成功后会触发 CHANNEL_CREATE 事件；
/// 分组类型（`ChannelKind::Category`）的 `position` 必须 >= 2。
#[derive(Debug, Default, Clone, Serialize)]
pub struct CreateChannelRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub channel_type: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_type: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_type: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_user_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speak_permission: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
}

impl CreateChannelRequest {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            ..Self::default()
        }
    }

    /// 文字子频道（type=0）
    pub fn text(name: impl Into<String>) -> Self {
        Self::new(name).with_type(ChannelKind::Text)
    }

    /// 语音子频道（type=2）
    pub fn voice(name: impl Into<String>) -> Self {
        Self::new(name).with_type(ChannelKind::Voice)
    }

    /// 分组（type=4）
    pub fn category(name: impl Into<String>) -> Self {
        Self::new(name).with_type(ChannelKind::Category)
    }

    pub fn with_type(mut self, kind: ChannelKind) -> Self {
        self.channel_type = Some(kind.raw());
        self
    }

    pub fn with_sub_type(mut self, sub_type: i32) -> Self {
        self.sub_type = Some(sub_type);
        self
    }

    pub fn with_position(mut self, position: i32) -> Self {
        self.position = Some(position);
        self
    }

    pub fn with_parent(mut self, parent_id: impl Into<String>) -> Self {
        self.parent_id = Some(parent_id.into());
        self
    }

    pub fn with_private_type(mut self, private_type: i32) -> Self {
        self.private_type = Some(private_type);
        self
    }

    pub fn with_private_user_ids(mut self, user_ids: Vec<String>) -> Self {
        self.private_user_ids = Some(user_ids);
        self
    }

    pub fn with_speak_permission(mut self, speak_permission: i32) -> Self {
        self.speak_permission = Some(speak_permission);
        self
    }

    pub fn with_application_id(mut self, application_id: impl Into<String>) -> Self {
        self.application_id = Some(application_id.into());
        self
    }
}

/// 修改子频道用户/身份组权限请求（PUT）。
///
/// 位图字符串：`1`=可查看、`2`=可管理、`4`=可发言；
/// 两个字段同一位都为 1 时表现为删除权限；本接口不支持修改「可管理子频道」。
#[derive(Debug, Default, Clone, Serialize)]
pub struct ChannelPermissionModify {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remove: Option<String>,
}

impl ChannelPermissionModify {
    pub fn new() -> Self {
        Self::default()
    }

    /// 授予可查看
    pub fn grant_view(self) -> Self {
        self.grant_bits(1)
    }

    /// 授予可发言
    pub fn grant_speak(self) -> Self {
        self.grant_bits(4)
    }

    /// 移除可查看
    pub fn revoke_view(self) -> Self {
        self.revoke_bits(1)
    }

    /// 移除可发言
    pub fn revoke_speak(self) -> Self {
        self.revoke_bits(4)
    }

    pub fn grant_bits(mut self, bits: u64) -> Self {
        self.add = Some(or_bits(self.add.as_deref(), bits));
        self
    }

    pub fn revoke_bits(mut self, bits: u64) -> Self {
        self.remove = Some(or_bits(self.remove.as_deref(), bits));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.add.is_none() && self.remove.is_none()
    }
}

fn or_bits(current: Option<&str>, bits: u64) -> String {
    let current = current
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    (current | bits).to_string()
}

/// 禁言请求（全员 / 指定成员共用）。
///
/// `mute_end_timestamp`（绝对秒级时间戳）与 `mute_seconds` 二选一，
/// 同时赋值以 `mute_end_timestamp` 为准；传 `"0"` 表示解除禁言。
#[derive(Debug, Default, Clone, Serialize)]
pub struct MuteRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute_end_timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute_seconds: Option<String>,
}

impl MuteRequest {
    pub fn new() -> Self {
        Self::default()
    }

    /// 禁言指定秒数
    pub fn seconds(seconds: u64) -> Self {
        Self {
            mute_seconds: Some(seconds.to_string()),
            ..Self::default()
        }
    }

    /// 禁言至绝对时间戳（秒）
    pub fn until(timestamp_secs: u64) -> Self {
        Self {
            mute_end_timestamp: Some(timestamp_secs.to_string()),
            ..Self::default()
        }
    }

    /// 解除禁言（mute_seconds="0"）
    pub fn unmute() -> Self {
        Self::seconds(0)
    }
}

/// 批量成员禁言请求（PATCH /guilds/{gid}/mute）。
#[derive(Debug, Clone, Serialize)]
pub struct MuteMembersRequest {
    #[serde(flatten)]
    pub mute: MuteRequest,
    pub user_ids: Vec<String>,
}

impl MuteMembersRequest {
    pub fn new(user_ids: Vec<String>, mute: MuteRequest) -> Self {
        Self { mute, user_ids }
    }
}

/// 批量成员禁言响应：设置成功的成员 id 列表。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MuteMembersResponse {
    #[serde(default)]
    pub user_ids: Vec<String>,
}

/// 删除频道成员请求体。
///
/// - `add_blacklist`：删除同时加入频道黑名单；
/// - `delete_history_msg_days`：撤回消息的时间范围，仅支持 `3/7/15/30`，
///   `-1` 表示撤回全部消息，`0`（默认）不撤回。
#[derive(Debug, Default, Clone, Serialize)]
pub struct DeleteMemberRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add_blacklist: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_history_msg_days: Option<i32>,
}

impl DeleteMemberRequest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn blacklist(mut self, add: bool) -> Self {
        self.add_blacklist = Some(add);
        self
    }

    /// 撤回历史消息天数（仅 `3/7/15/30`，`-1`=全部）
    pub fn delete_history_days(mut self, days: i32) -> Self {
        self.delete_history_msg_days = Some(days);
        self
    }
}

/// API 权限需求标识（path + method）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiPermissionDemandIdentify {
    pub path: String,
    pub method: String,
}

impl ApiPermissionDemandIdentify {
    pub fn new(path: impl Into<String>, method: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            method: method.into(),
        }
    }
}

/// 创建 API 权限授权链接请求（POST /guilds/{gid}/api_permission/demand）。
#[derive(Debug, Clone, Serialize)]
pub struct CreateApiPermissionDemandRequest {
    pub channel_id: String,
    pub api_identify: ApiPermissionDemandIdentify,
    pub desc: String,
}

impl CreateApiPermissionDemandRequest {
    pub fn new(
        channel_id: impl Into<String>,
        api_identify: ApiPermissionDemandIdentify,
        desc: impl Into<String>,
    ) -> Self {
        Self {
            channel_id: channel_id.into(),
            api_identify,
            desc: desc.into(),
        }
    }
}

/// API 权限授权链接对象（POST 响应）。
#[derive(Debug, Clone, Deserialize)]
pub struct ApiPermissionDemand {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub api_identify: Option<ApiPermissionDemandIdentify>,
    pub title: Option<String>,
    pub desc: Option<String>,
}

/// 推荐子频道项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendChannel {
    pub channel_id: String,
    pub introduce: String,
}

impl RecommendChannel {
    pub fn new(channel_id: impl Into<String>, introduce: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            introduce: introduce.into(),
        }
    }
}

/// 公告类型：0=成员公告（默认），1=欢迎公告。
pub const ANNOUNCE_TYPE_MEMBER: u32 = 0;
pub const ANNOUNCE_TYPE_WELCOME: u32 = 1;

/// 创建频道全局公告请求（POST /guilds/{gid}/announces）。
///
/// - `message_id` 有值时创建消息类型公告（此时 `channel_id` 必填）；
/// - `recommend_channels` 有值时创建推荐子频道公告（message_id 留空），
///   会一次全部替换推荐子频道列表。
#[derive(Debug, Default, Clone, Serialize)]
pub struct CreateAnnounceRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announces_type: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommend_channels: Option<Vec<RecommendChannel>>,
}

impl CreateAnnounceRequest {
    /// 将某条消息设置为成员公告
    pub fn message(channel_id: impl Into<String>, message_id: impl Into<String>) -> Self {
        Self {
            message_id: Some(message_id.into()),
            channel_id: Some(channel_id.into()),
            announces_type: Some(ANNOUNCE_TYPE_MEMBER),
            recommend_channels: None,
        }
    }

    /// 推荐子频道公告（会全部替换现有推荐子频道）
    pub fn recommend(channels: Vec<RecommendChannel>) -> Self {
        Self {
            message_id: None,
            channel_id: None,
            announces_type: Some(ANNOUNCE_TYPE_WELCOME),
            recommend_channels: Some(channels),
        }
    }

    pub fn with_announces_type(mut self, announces_type: u32) -> Self {
        self.announces_type = Some(announces_type);
        self
    }
}

/// 频道公告对象（创建响应）。
#[derive(Debug, Clone, Deserialize)]
pub struct Announces {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    pub announces_type: Option<u32>,
    #[serde(default)]
    pub recommend_channels: Vec<RecommendChannel>,
}

/// 音频播放状态（官方 STATUS）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum AudioStatus {
    /// 开始播放
    Start = 0,
    /// 暂停播放
    Pause = 1,
    /// 继续播放
    Resume = 2,
    /// 停止播放
    Stop = 3,
}

/// 音频控制请求（POST /channels/{cid}/audio）。
///
/// ⚠️ 仅音频类机器人可用，需联系平台申请权限。
#[derive(Debug, Clone, Serialize)]
pub struct AudioControl {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub status: AudioStatus,
}

impl AudioControl {
    /// 开始播放（audio_url 必填，text 可选）
    pub fn play(audio_url: impl Into<String>, text: Option<String>) -> Self {
        Self {
            audio_url: Some(audio_url.into()),
            text,
            status: AudioStatus::Start,
        }
    }

    pub fn pause() -> Self {
        Self {
            audio_url: None,
            text: None,
            status: AudioStatus::Pause,
        }
    }

    pub fn resume() -> Self {
        Self {
            audio_url: None,
            text: None,
            status: AudioStatus::Resume,
        }
    }

    pub fn stop() -> Self {
        Self {
            audio_url: None,
            text: None,
            status: AudioStatus::Stop,
        }
    }
}

/// 论坛帖子文本格式（官方 Format）。
pub const THREAD_FORMAT_TEXT: u32 = 1;
pub const THREAD_FORMAT_HTML: u32 = 2;
pub const THREAD_FORMAT_MARKDOWN: u32 = 3;
pub const THREAD_FORMAT_JSON: u32 = 4;

/// 论坛主帖内容。
#[derive(Debug, Clone, Deserialize)]
pub struct ThreadInfo {
    pub thread_id: Option<String>,
    pub title: Option<String>,
    pub content: Option<String>,
    pub date_time: Option<String>,
}

/// 论坛帖子（主帖）。
#[derive(Debug, Clone, Deserialize)]
pub struct Thread {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub author_id: Option<String>,
    pub thread_info: Option<ThreadInfo>,
}

/// 帖子列表响应：`{threads: [...], is_finish: 0|1}`。
#[derive(Debug, Clone, Deserialize)]
pub struct ThreadsPage {
    #[serde(default)]
    pub threads: Vec<Thread>,
    #[serde(default, deserialize_with = "de::lenient_i64")]
    pub is_finish: i64,
}

impl ThreadsPage {
    /// 是否已拉取完毕（平台值 1 表示是）
    pub fn is_finish(&self) -> bool {
        self.is_finish != 0
    }
}

/// 发表帖子请求（PUT /channels/{cid}/threads）。
#[derive(Debug, Clone, Serialize)]
pub struct CreateThreadRequest {
    pub title: String,
    pub content: String,
    pub format: u32,
}

impl CreateThreadRequest {
    pub fn new(title: impl Into<String>, content: impl Into<String>, format: u32) -> Self {
        Self {
            title: title.into(),
            content: content.into(),
            format,
        }
    }

    pub fn markdown(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(title, content, THREAD_FORMAT_MARKDOWN)
    }

    pub fn text(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(title, content, THREAD_FORMAT_TEXT)
    }
}

/// 发表帖子响应：任务 ID 与发帖时间戳（秒）。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateThreadResponse {
    pub task_id: String,
    pub create_time: Option<String>,
}

/// 修改日程请求（PATCH /channels/{cid}/schedules/{sid}）。
///
/// 响应为修改后的 [`Schedule`]；请求自动包 `{"schedule": {...}}` 信封。
#[derive(Debug, Default, Clone, Serialize)]
pub struct PatchScheduleRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser::opt_i64_as_string"
    )]
    pub start_timestamp: Option<i64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser::opt_i64_as_string"
    )]
    pub end_timestamp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jump_channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remind_type: Option<RemindType>,
}

impl PatchScheduleRequest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_time_range(mut self, start_timestamp_ms: i64, end_timestamp_ms: i64) -> Self {
        self.start_timestamp = Some(start_timestamp_ms);
        self.end_timestamp = Some(end_timestamp_ms);
        self
    }

    pub fn with_jump_channel(mut self, channel_id: impl Into<String>) -> Self {
        self.jump_channel_id = Some(channel_id.into());
        self
    }

    pub fn with_remind_type(mut self, remind_type: RemindType) -> Self {
        self.remind_type = Some(remind_type);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.description.is_none()
            && self.start_timestamp.is_none()
            && self.end_timestamp.is_none()
            && self.jump_channel_id.is_none()
            && self.remind_type.is_none()
    }

    pub(crate) fn envelope(schedule: &PatchScheduleRequest) -> impl Serialize + use<'_> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            schedule: &'a PatchScheduleRequest,
        }
        Envelope { schedule }
    }
}

/// 创建私信会话请求（POST /users/@me/dms）。
#[derive(Debug, Clone, Serialize)]
pub struct CreateDmsRequest {
    pub recipient_id: String,
    pub source_guild_id: String,
}

impl CreateDmsRequest {
    pub fn new(recipient_id: impl Into<String>, source_guild_id: impl Into<String>) -> Self {
        Self {
            recipient_id: recipient_id.into(),
            source_guild_id: source_guild_id.into(),
        }
    }
}

/// 私信会话对象。
#[derive(Debug, Clone, Deserialize)]
pub struct DmsSession {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub create_time: Option<String>,
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
    pub edited_timestamp: Option<String>,
    #[serde(default, deserialize_with = "de::opt_lenient_bool")]
    pub mention_everyone: Option<bool>,
    pub author: Option<UserInfo>,
    pub member: Option<Member>,
    #[serde(default)]
    pub attachments: Vec<MessageAttachment>,
    #[serde(default)]
    pub embeds: Vec<MessageEmbed>,
    #[serde(default)]
    pub mentions: Vec<UserInfo>,
    pub ark: Option<MessageArk>,
    #[serde(default, deserialize_with = "de::opt_lenient_i64")]
    pub seq: Option<i64>,
    pub seq_in_channel: Option<String>,
    pub message_reference: Option<MessageReference>,
}

/// 消息附件（发送消息成功后平台转存产生）。
#[derive(Debug, Clone, Deserialize)]
pub struct MessageAttachment {
    pub url: Option<String>,
}

/// embed 消息对象（官方《消息对象(Message)》MessageEmbed）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageEmbed {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<MessageEmbedThumbnail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<MessageEmbedField>>,
}

impl MessageEmbed {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            ..Self::default()
        }
    }

    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = Some(prompt.into());
        self
    }

    pub fn with_thumbnail(mut self, url: impl Into<String>) -> Self {
        self.thumbnail = Some(MessageEmbedThumbnail { url: url.into() });
        self
    }

    pub fn with_field(mut self, name: impl Into<String>) -> Self {
        self.fields
            .get_or_insert_with(Vec::new)
            .push(MessageEmbedField { name: name.into() });
        self
    }
}

/// embed 缩略图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEmbedThumbnail {
    pub url: String,
}

/// embed 字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEmbedField {
    pub name: String,
}

/// ark 消息对象。`template_id` 需要在开放平台申请。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageArk {
    pub template_id: i32,
    pub kv: Vec<MessageArkKv>,
}

/// ark kv 项：`value` 与 `obj` 二选一。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageArkKv {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub obj: Option<Vec<MessageArkObj>>,
}

impl MessageArkKv {
    pub fn value(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: Some(value.into()),
            obj: None,
        }
    }
}

/// ark obj 列表项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageArkObj {
    pub obj_kv: Vec<MessageArkObjKv>,
}

/// ark objkv 键值对。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageArkObjKv {
    pub key: String,
    pub value: String,
}

impl MessageArkObjKv {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// 消息引用（引用回复）。`message_id` 为要引用的消息 id。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageReference {
    pub message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_get_message_error: Option<bool>,
}

impl MessageReference {
    pub fn new(message_id: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            ignore_get_message_error: None,
        }
    }

    pub fn ignore_error(mut self, ignore: bool) -> Self {
        self.ignore_get_message_error = Some(ignore);
        self
    }
}

/// 频道 markdown 消息对象（官方《消息对象(Message)》MessageMarkdown）。
///
/// `content`（原生 markdown）与 `template_id` + `params`（模板）互斥。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageMarkdown {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Vec<MessageMarkdownParam>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl MessageMarkdown {
    /// 原生 markdown 内容（与模板互斥）
    pub fn native(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            ..Self::default()
        }
    }

    /// 模板 markdown：模板 id + 变量
    pub fn template(template_id: i32, params: Vec<MessageMarkdownParam>) -> Self {
        Self {
            template_id: Some(template_id),
            params: Some(params),
            content: None,
        }
    }
}

/// markdown 模板变量：key 与 values 列表一一对应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMarkdownParam {
    pub key: String,
    pub values: Vec<String>,
}

impl MessageMarkdownParam {
    pub fn new(key: impl Into<String>, values: Vec<String>) -> Self {
        Self {
            key: key.into(),
            values,
        }
    }

    pub fn single(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new(key, vec![value.into()])
    }
}

/// 消息审核结果事件对象（MESSAGE_AUDIT_PASS / MESSAGE_AUDIT_REJECT）。
#[derive(Debug, Clone, Deserialize)]
pub struct MessageAudited {
    pub audit_id: Option<String>,
    pub message_id: Option<String>,
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub audit_time: Option<String>,
    pub create_time: Option<String>,
    pub seq_in_channel: Option<String>,
}

impl MessageAudited {
    /// 审核通过的事件才携带 message_id
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref().filter(|s| !s.is_empty())
    }
}

/// 发送频道消息请求体。
///
/// - 带 `msg_id` 或 `event_id` 即被动回复（有效期 5 分钟、最多回复 5 次）；
/// - 不带即主动消息，会进入安全审核通道并消耗月度配额，第一版不建议依赖；
/// - `content, embed, ark, image/file_image, markdown 至少需要有一个字段`，
///   否则平台无法下发消息（`get_message_setting` 可查频率设置）。
#[derive(Debug, Default, Clone, Serialize)]
pub struct ChannelMessageRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embed: Option<MessageEmbed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ark: Option<MessageArk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_reference: Option<MessageReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MessageMarkdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
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

    /// 构造事件被动回复（携带 event_id）
    pub fn reply_event(event_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            event_id: Some(event_id.into()),
            ..Self::default()
        }
    }

    pub fn with_image(mut self, url: impl Into<String>) -> Self {
        self.image = Some(url.into());
        self
    }

    pub fn with_embed(mut self, embed: MessageEmbed) -> Self {
        self.embed = Some(embed);
        self
    }

    pub fn with_ark(mut self, ark: MessageArk) -> Self {
        self.ark = Some(ark);
        self
    }

    pub fn with_markdown(mut self, markdown: MessageMarkdown) -> Self {
        self.markdown = Some(markdown);
        self
    }

    pub fn with_message_reference(mut self, reference: MessageReference) -> Self {
        self.message_reference = Some(reference);
        self
    }

    /// 设置 msg_seq（同一 msg_id 的第 n 次回复，从 1 开始）
    pub fn with_msg_seq(mut self, msg_seq: i32) -> Self {
        self.msg_seq = Some(msg_seq);
        self
    }

    /// 是否至少包含一个平台要求的下发内容字段
    pub fn has_sendable_content(&self) -> bool {
        self.content.is_some()
            || self.image.is_some()
            || self.embed.is_some()
            || self.ark.is_some()
            || self.markdown.is_some()
    }

    /// 构造 `multipart/form-data` 表单（对象/数组字段序列化为 JSON 字符串）。
    pub(crate) fn to_multipart_form(&self) -> Result<reqwest::multipart::Form, GuildError> {
        let to_json = |value: &serde_json::Value| -> Result<String, GuildError> {
            serde_json::to_string(value).map_err(GuildError::Json)
        };

        let mut form = reqwest::multipart::Form::new();
        if let Some(content) = &self.content {
            form = form.text("content", content.clone());
        }
        if let Some(image) = &self.image {
            form = form.text("image", image.clone());
        }
        if let Some(msg_id) = &self.msg_id {
            form = form.text("msg_id", msg_id.clone());
        }
        if let Some(event_id) = &self.event_id {
            form = form.text("event_id", event_id.clone());
        }
        if let Some(msg_seq) = self.msg_seq {
            form = form.text("msg_seq", msg_seq.to_string());
        }
        if let Some(embed) = &self.embed {
            form = form.text("embed", to_json(&serde_json::to_value(embed)?)?);
        }
        if let Some(ark) = &self.ark {
            form = form.text("ark", to_json(&serde_json::to_value(ark)?)?);
        }
        if let Some(markdown) = &self.markdown {
            form = form.text("markdown", to_json(&serde_json::to_value(markdown)?)?);
        }
        if let Some(reference) = &self.message_reference {
            form = form.text(
                "message_reference",
                to_json(&serde_json::to_value(reference)?)?,
            );
        }
        Ok(form)
    }
}

/// 以 `multipart/form-data` 上传的图片文件。
///
/// 官方说明：form-data 支持直接通过文件上传的方式发送图片（`file_image` 字段）；
/// 对象/数组字段需序列化为 JSON 字符串后携带。
#[derive(Debug, Clone)]
pub struct ChannelMessageFile {
    pub file_name: String,
    pub bytes: Vec<u8>,
    pub mime_type: Option<String>,
}

impl ChannelMessageFile {
    pub fn new(file_name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            file_name: file_name.into(),
            bytes,
            mime_type: None,
        }
    }

    pub fn with_mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }
}

/// 主动消息安全审核信息（HTTP 200 但未即时送达）。
///
/// 平台存在两种形态（均已兼容）：
/// - 实测场景：body 顶层带 `code=304023` + `audit_id`；
/// - 官方文档：`{"code":304023,"message":"...","data":{"message_audit":{"audit_id":"..."}}}`。
#[derive(Debug, Clone, Deserialize)]
pub struct MessageAuditInfo {
    #[serde(default, deserialize_with = "de::opt_lenient_i64")]
    pub code: Option<i64>,
    pub message: Option<String>,
    pub audit_id: Option<String>,
}

impl MessageAuditInfo {
    /// 从任意 2xx 响应体中识别审核响应；非审核响应返回 None。
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        let code = value.get("code").and_then(|c| c.as_i64());
        let message = value
            .get("message")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());
        let audit_id = value
            .get("audit_id")
            .and_then(|a| a.as_str())
            .or_else(|| {
                value
                    .pointer("/data/message_audit/audit_id")
                    .and_then(|a| a.as_str())
            })
            .map(|s| s.to_string());

        let is_audit = audit_id.is_some() || matches!(code, Some(304023 | 304024));
        is_audit.then_some(Self {
            code,
            message,
            audit_id,
        })
    }
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

    // 主动消息安全审核：顶层 audit_id 或 data.message_audit.audit_id 均可识别
    if let Some(info) = MessageAuditInfo::from_value(&value) {
        warn!(
            audit_id = ?info.audit_id,
            code = ?info.code,
            "主动消息进入安全审核，未即时送达"
        );
        return Ok(ChannelMessageSendOutcome::Auditing(info));
    }

    match &value {
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

/// 表情表态类型（官方《表情对象》EmojiType）：`1`=系统表情，`2`=emoji 表情。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum ReactionType {
    SystemFace = 1,
    Emoji = 2,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReactionUser {
    pub id: String,
    pub username: Option<String>,
    pub avatar: Option<String>,
}

/// 表情表态用户列表页（cookie 游标分页）。
///
/// 官方字段名是 `users`；兼容部分返回 `data` 的历史形态。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReactionUsersPage {
    #[serde(default, alias = "data")]
    pub users: Vec<ReactionUser>,
    #[serde(default)]
    pub cookie: Option<String>,
    #[serde(default, deserialize_with = "de::opt_lenient_bool")]
    pub is_end: Option<bool>,
}

impl ReactionUsersPage {
    /// 兼容历史字段名 `data` 的读取入口
    pub fn data(&self) -> &[ReactionUser] {
        &self.users
    }

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

    /// Option<i64> 序列化为字符串（None 跳过，由外层 skip_serializing_if 保证）
    pub fn opt_i64_as_string<S: Serializer>(
        value: &Option<i64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => serializer.serialize_str(&v.to_string()),
            None => serializer.serialize_none(),
        }
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

    async fn send_multipart_request(
        &self,
        method: Method,
        url: &str,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::Response, GuildError> {
        let authorization = self.auth_client.get_authorization().await?;
        debug!(method = %method, url = %url, "发送multipart请求");
        Ok(self
            .http_client
            .request(method, url)
            .header("Authorization", &authorization)
            .multipart(form)
            .send()
            .await?)
    }

    /// 统一解析消息发送响应（含主动消息审核识别）。
    async fn parse_send_response(
        response: reqwest::Response,
    ) -> Result<ChannelMessageSendOutcome, GuildError> {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            error!(status = %status, body = %text, "发送消息失败");
            return Err(GuildError::Api(text));
        }

        debug!(status = %status, body = %text, "发送消息响应");
        classify_send_body(&text).map_err(GuildError::Json)
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

    /// 创建子频道（POST /guilds/{gid}/channels）。
    ///
    /// 官方说明：私域接口，创建成功后会触发 CHANNEL_CREATE 事件；
    /// 实测当前机器人资质不足时返回 `200010 create channel error`
    /// （见 docs/API_VERIFICATION.md §三），接口本身已按文档实现。
    pub async fn create_channel(
        &self,
        guild_id: &str,
        request: CreateChannelRequest,
    ) -> Result<ChannelInfo, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/channels");
        let response = self.send_json_request(Method::POST, &url, &request).await?;
        let channel: ChannelInfo = parse_response::<_, GuildError>(response).await?;
        info!(guild_id = %guild_id, channel_id = %channel.id, "创建子频道成功");
        Ok(channel)
    }

    /// 删除子频道（DELETE /channels/{cid}，成功返回 200/空体）。
    ///
    /// ⚠️ 私域接口，删除成功后会触发 CHANNEL_DELETE 事件，子频道删除后无法恢复。
    pub async fn delete_channel(&self, channel_id: &str) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}");
        let response = self.send_request(Method::DELETE, &url).await?;
        info!(channel_id = %channel_id, "删除子频道成功");
        parse_empty_response::<GuildError>(response).await
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

    /// 删除频道成员（DELETE /guilds/{gid}/members/{uid}，成功返回 204）。
    ///
    /// - 机器人需被添加为管理员，且无法移除身份为管理员的成员；
    /// - `request` 可设置删除同时加入黑名单、撤回历史消息范围。
    pub async fn delete_member(
        &self,
        guild_id: &str,
        user_id: &str,
        request: DeleteMemberRequest,
    ) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/members/{user_id}");
        let response = self
            .send_json_request(Method::DELETE, &url, &request)
            .await?;
        info!(guild_id = %guild_id, user_id = %user_id, "删除频道成员成功");
        parse_empty_response::<GuildError>(response).await
    }

    /// 频道全员禁言 / 解除禁言（PATCH /guilds/{gid}/mute，成功返回 204）。
    ///
    /// `MuteRequest::unmute()` 可解除禁言。
    pub async fn mute_guild(&self, guild_id: &str, request: MuteRequest) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/mute");
        let response = self
            .send_json_request(Method::PATCH, &url, &request)
            .await?;
        info!(guild_id = %guild_id, "设置频道全员禁言成功");
        parse_empty_response::<GuildError>(response).await
    }

    /// 频道指定成员禁言 / 解除禁言
    /// （PATCH /guilds/{gid}/members/{uid}/mute，成功返回 204）。
    pub async fn mute_member(
        &self,
        guild_id: &str,
        user_id: &str,
        request: MuteRequest,
    ) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/members/{user_id}/mute");
        let response = self
            .send_json_request(Method::PATCH, &url, &request)
            .await?;
        info!(guild_id = %guild_id, user_id = %user_id, "设置频道成员禁言成功");
        parse_empty_response::<GuildError>(response).await
    }

    /// 频道批量成员禁言 / 批量解除禁言（PATCH /guilds/{gid}/mute）。
    ///
    /// 成功返回本次设置成功的成员 id 列表（HTTP 200）。
    pub async fn mute_members(
        &self,
        guild_id: &str,
        request: MuteMembersRequest,
    ) -> Result<MuteMembersResponse, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/mute");
        let response = self
            .send_json_request(Method::PATCH, &url, &request)
            .await?;
        let status = response.status();
        let text = read_body_text(response).await;
        if !status.is_success() {
            error!(status = %status, body = %text, "批量设置成员禁言失败");
            return Err(GuildError::Api(text));
        }
        // 实测：空 user_ids 时平台返回空 body，需按空结果容忍
        if is_nullish(&text) {
            info!(guild_id = %guild_id, "批量设置成员禁言返回空响应");
            return Ok(MuteMembersResponse::default());
        }
        let result: MuteMembersResponse = serde_json::from_str(&text)?;
        info!(
            guild_id = %guild_id,
            requested = request.user_ids.len(),
            affected = result.user_ids.len(),
            "批量设置成员禁言成功"
        );
        Ok(result)
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

    /// 创建频道身份组成员
    /// （PUT /guilds/{gid}/members/{uid}/roles/{rid}，成功返回 204）。
    ///
    /// 若添加的身份组是 `5-子频道管理员`，必须通过 `channel_id` 指定具体子频道。
    pub async fn add_member_role(
        &self,
        guild_id: &str,
        user_id: &str,
        role_id: &str,
        channel_id: Option<&str>,
    ) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/members/{user_id}/roles/{role_id}");
        let response = match channel_id {
            Some(channel_id) => {
                #[derive(Serialize)]
                struct ChannelRef<'a> {
                    id: &'a str,
                }
                #[derive(Serialize)]
                struct Body<'a> {
                    channel: ChannelRef<'a>,
                }
                let body = Body {
                    channel: ChannelRef { id: channel_id },
                };
                self.send_json_request(Method::PUT, &url, &body).await?
            }
            None => self.send_request(Method::PUT, &url).await?,
        };
        info!(guild_id = %guild_id, user_id = %user_id, role_id = %role_id, "创建身份组成员成功");
        parse_empty_response::<GuildError>(response).await
    }

    /// 删除频道身份组成员
    /// （DELETE /guilds/{gid}/members/{uid}/roles/{rid}，成功返回 204）。
    ///
    /// 若删除的身份组是 `5-子频道管理员`，必须通过 `channel_id` 指定具体子频道。
    pub async fn remove_member_role(
        &self,
        guild_id: &str,
        user_id: &str,
        role_id: &str,
        channel_id: Option<&str>,
    ) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/members/{user_id}/roles/{role_id}");
        let response = match channel_id {
            Some(channel_id) => {
                #[derive(Serialize)]
                struct ChannelRef<'a> {
                    id: &'a str,
                }
                #[derive(Serialize)]
                struct Body<'a> {
                    channel: ChannelRef<'a>,
                }
                let body = Body {
                    channel: ChannelRef { id: channel_id },
                };
                self.send_json_request(Method::DELETE, &url, &body).await?
            }
            None => self.send_request(Method::DELETE, &url).await?,
        };
        info!(guild_id = %guild_id, user_id = %user_id, role_id = %role_id, "删除身份组成员成功");
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

    /// 修改子频道指定用户权限
    /// （PUT /channels/{cid}/members/{uid}/permissions，成功返回 204）。
    ///
    /// 本接口不支持修改「可管理子频道」权限；同一位 `add` 与 `remove` 都为 1
    /// 时表现为删除权限。
    pub async fn update_channel_user_permissions(
        &self,
        channel_id: &str,
        user_id: &str,
        request: ChannelPermissionModify,
    ) -> Result<(), GuildError> {
        if request.is_empty() {
            return Err(GuildError::InvalidRequest(
                "修改子频道权限请求不能为空，至少设置 add 或 remove".to_string(),
            ));
        }
        let url = format!("{API_BASE_URL}/channels/{channel_id}/members/{user_id}/permissions");
        let response = self.send_json_request(Method::PUT, &url, &request).await?;
        info!(channel_id = %channel_id, user_id = %user_id, "修改子频道用户权限成功");
        parse_empty_response::<GuildError>(response).await
    }

    /// 修改子频道指定身份组权限
    /// （PUT /channels/{cid}/roles/{rid}/permissions，成功返回 204）。
    pub async fn update_channel_role_permissions(
        &self,
        channel_id: &str,
        role_id: &str,
        request: ChannelPermissionModify,
    ) -> Result<(), GuildError> {
        if request.is_empty() {
            return Err(GuildError::InvalidRequest(
                "修改子频道权限请求不能为空，至少设置 add 或 remove".to_string(),
            ));
        }
        let url = format!("{API_BASE_URL}/channels/{channel_id}/roles/{role_id}/permissions");
        let response = self.send_json_request(Method::PUT, &url, &request).await?;
        info!(channel_id = %channel_id, role_id = %role_id, "修改子频道身份组权限成功");
        parse_empty_response::<GuildError>(response).await
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

    /// 发送机器人接口权限授权链接
    /// （POST /guilds/{gid}/api_permission/demand，每天每频道默认限 3 条）。
    pub async fn create_api_permission_demand(
        &self,
        guild_id: &str,
        request: CreateApiPermissionDemandRequest,
    ) -> Result<ApiPermissionDemand, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/api_permission/demand");
        let response = self.send_json_request(Method::POST, &url, &request).await?;
        info!(guild_id = %guild_id, "创建API权限授权链接成功");
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
    /// - 被动回复（带 `msg_id` 或 `event_id`）：即时送达，`msg_id` 有效期 5 分钟，
    ///   同一 `msg_id` 最多回复 5 次；
    /// - 主动消息：可能进入安全审核，用 [`ChannelMessageSendOutcome`] 判断；
    /// - 支持 content / image / embed / ark / markdown，至少需要一项；
    ///   本地图片请使用 [`Self::send_channel_message_multipart`]。
    pub async fn send_channel_message(
        &self,
        channel_id: &str,
        request: ChannelMessageRequest,
    ) -> Result<ChannelMessageSendOutcome, GuildError> {
        if !request.has_sendable_content() {
            return Err(GuildError::InvalidRequest(
                "消息至少需要 content/image/embed/ark/markdown 之一".to_string(),
            ));
        }
        let url = format!("{API_BASE_URL}/channels/{channel_id}/messages");
        let response = self.send_json_request(Method::POST, &url, &request).await?;
        Self::parse_send_response(response).await
    }

    /// 以 `multipart/form-data` 发送带本地图片的频道消息
    /// （对应官方 `file_image` 字段，其余对象字段会序列化为 JSON 字符串）。
    pub async fn send_channel_message_multipart(
        &self,
        channel_id: &str,
        request: &ChannelMessageRequest,
        file: ChannelMessageFile,
    ) -> Result<ChannelMessageSendOutcome, GuildError> {
        let mut form = request.to_multipart_form()?;
        let mut part = reqwest::multipart::Part::bytes(file.bytes).file_name(file.file_name);
        if let Some(mime_type) = file.mime_type {
            part = part
                .mime_str(&mime_type)
                .map_err(|e| GuildError::InvalidRequest(format!("非法 MIME 类型: {e}")))?;
        }
        form = form.part("file_image", part);

        let url = format!("{API_BASE_URL}/channels/{channel_id}/messages");
        let response = self
            .send_multipart_request(Method::POST, &url, form)
            .await?;
        Self::parse_send_response(response).await
    }

    /// 创建频道私信会话（POST /users/@me/dms）。
    ///
    /// 机器人与用户存在共同频道才能创建；成功返回私信 `guild_id`/`channel_id`。
    pub async fn create_dms_session(
        &self,
        recipient_id: &str,
        source_guild_id: &str,
    ) -> Result<DmsSession, GuildError> {
        let url = format!("{API_BASE_URL}/users/@me/dms");
        let request = CreateDmsRequest::new(recipient_id, source_guild_id);
        let response = self.send_json_request(Method::POST, &url, &request).await?;
        let session: DmsSession = parse_response::<_, GuildError>(response).await?;
        info!(recipient_id = %recipient_id, guild_id = ?session.guild_id, "创建私信会话成功");
        Ok(session)
    }

    /// 发送私信消息（POST /dms/{guild_id}/messages），参数与返回同频道消息。
    pub async fn send_dms_message(
        &self,
        dms_guild_id: &str,
        request: ChannelMessageRequest,
    ) -> Result<ChannelMessageSendOutcome, GuildError> {
        if !request.has_sendable_content() {
            return Err(GuildError::InvalidRequest(
                "消息至少需要 content/image/embed/ark/markdown 之一".to_string(),
            ));
        }
        let url = format!("{API_BASE_URL}/dms/{dms_guild_id}/messages");
        let response = self.send_json_request(Method::POST, &url, &request).await?;
        Self::parse_send_response(response).await
    }

    /// 撤回私信消息（DELETE /dms/{guild_id}/messages/{message_id}?hidetip=false）。
    pub async fn recall_dms_message(
        &self,
        dms_guild_id: &str,
        message_id: &str,
        hide_tip: bool,
    ) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/dms/{dms_guild_id}/messages/{message_id}");
        let authorization = self.auth_client.get_authorization().await?;
        let response = self
            .http_client
            .delete(&url)
            .header("Authorization", &authorization)
            .query(&[("hidetip", hide_tip.to_string())])
            .send()
            .await?;
        info!(dms_guild_id = %dms_guild_id, message_id = %message_id, "撤回私信消息成功");
        parse_empty_response::<GuildError>(response).await
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
                users: Vec::new(),
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

    /// 修改日程（PATCH /channels/{cid}/schedules/{sid}）。
    ///
    /// 请求自动包 `{"schedule": {...}}` 信封；时间戳为毫秒字符串。
    pub async fn patch_schedule(
        &self,
        channel_id: &str,
        schedule_id: &str,
        request: PatchScheduleRequest,
    ) -> Result<Option<Schedule>, GuildError> {
        if request.is_empty() {
            return Err(GuildError::InvalidRequest(
                "修改日程请求不能为空，至少设置一项字段".to_string(),
            ));
        }
        let url = format!("{API_BASE_URL}/channels/{channel_id}/schedules/{schedule_id}");
        let envelope = PatchScheduleRequest::envelope(&request);
        let response = self
            .send_json_request(Method::PATCH, &url, &envelope)
            .await?;
        let text = read_body_text(response).await;
        if is_nullish(&text) {
            warn!("修改日程返回空响应");
            return Ok(None);
        }
        info!(channel_id = %channel_id, schedule_id = %schedule_id, "修改日程成功");
        Ok(Some(serde_json::from_str(&text)?))
    }

    // -- 第二档：内容管理（公告 / 音频 / 论坛） ------------------------------------

    /// 创建频道全局公告（POST /guilds/{gid}/announces）。
    ///
    /// - 消息类型公告：[`CreateAnnounceRequest::message`]；
    /// - 推荐子频道公告：[`CreateAnnounceRequest::recommend`]（最多 3 条，会全部替换）；
    /// - 删除推荐子频道公告请将 message_id 设为 `all`（[`Self::delete_announce`]）。
    pub async fn create_announce(
        &self,
        guild_id: &str,
        request: CreateAnnounceRequest,
    ) -> Result<Announces, GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/announces");
        let response = self.send_json_request(Method::POST, &url, &request).await?;
        info!(guild_id = %guild_id, "创建频道公告成功");
        parse_response::<_, GuildError>(response).await
    }

    /// 删除频道全局公告（DELETE /guilds/{gid}/announces/{message_id}，成功返回 204）。
    ///
    /// 不校验 `message_id` 合法性时传字面量 `"all"`。
    pub async fn delete_announce(
        &self,
        guild_id: &str,
        message_id: &str,
    ) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/guilds/{guild_id}/announces/{message_id}");
        let response = self.send_request(Method::DELETE, &url).await?;
        info!(guild_id = %guild_id, message_id = %message_id, "删除频道公告成功");
        parse_empty_response::<GuildError>(response).await
    }

    /// 音频控制（POST /channels/{cid}/audio）。
    ///
    /// ⚠️ 仅音频类机器人可用，需联系平台申请权限（实测未覆盖）。
    pub async fn audio_control(
        &self,
        channel_id: &str,
        request: AudioControl,
    ) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/audio");
        let response = self.send_json_request(Method::POST, &url, &request).await?;
        info!(channel_id = %channel_id, status = ?request.status, "音频控制成功");
        parse_empty_response::<GuildError>(response).await
    }

    /// 机器人在语音子频道上麦（PUT /channels/{cid}/mic，成功返回空对象）。
    pub async fn put_mic(&self, channel_id: &str) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/mic");
        let response = self.send_request(Method::PUT, &url).await?;
        info!(channel_id = %channel_id, "机器人上麦成功");
        parse_empty_response::<GuildError>(response).await
    }

    /// 机器人在语音子频道下麦（DELETE /channels/{cid}/mic，成功返回空对象）。
    pub async fn delete_mic(&self, channel_id: &str) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/mic");
        let response = self.send_request(Method::DELETE, &url).await?;
        info!(channel_id = %channel_id, "机器人下麦成功");
        parse_empty_response::<GuildError>(response).await
    }

    /// 获取子频道帖子列表（GET /channels/{cid}/threads）。
    ///
    /// ⚠️ 私域接口，仅私域机器人可用。
    pub async fn get_threads(&self, channel_id: &str) -> Result<ThreadsPage, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/threads");
        let response = self.send_request(Method::GET, &url).await?;
        parse_response::<_, GuildError>(response).await
    }

    /// 获取帖子详情（GET /channels/{cid}/threads/{thread_id}，响应包一层 `{thread:...}`）。
    pub async fn get_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
    ) -> Result<Thread, GuildError> {
        #[derive(Deserialize)]
        struct Wrapper {
            thread: Thread,
        }
        let url = format!("{API_BASE_URL}/channels/{channel_id}/threads/{thread_id}");
        let response = self.send_request(Method::GET, &url).await?;
        let text = read_body_text(response).await;
        match serde_json::from_str::<Wrapper>(&text) {
            Ok(wrapper) => Ok(wrapper.thread),
            Err(_) => Ok(serde_json::from_str::<Thread>(&text)?),
        }
    }

    /// 发表帖子（PUT /channels/{cid}/threads），返回发帖任务 ID。
    pub async fn create_thread(
        &self,
        channel_id: &str,
        request: CreateThreadRequest,
    ) -> Result<CreateThreadResponse, GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/threads");
        let response = self.send_json_request(Method::PUT, &url, &request).await?;
        info!(channel_id = %channel_id, "发表帖子成功");
        parse_response::<_, GuildError>(response).await
    }

    /// 删除帖子（DELETE /channels/{cid}/threads/{thread_id}，成功返回 204）。
    pub async fn delete_thread(&self, channel_id: &str, thread_id: &str) -> Result<(), GuildError> {
        let url = format!("{API_BASE_URL}/channels/{channel_id}/threads/{thread_id}");
        let response = self.send_request(Method::DELETE, &url).await?;
        info!(channel_id = %channel_id, thread_id = %thread_id, "删除帖子成功");
        parse_empty_response::<GuildError>(response).await
    }

    // -- 第二档：在线人数 ---------------------------------------------------------

    /// 子频道在线人数（GET /channels/{cid}/online_nums）。
    ///
    /// ⚠️ 实测仅语音/直播子频道(type=2/10005)有效；文字子频道报 505007；
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
        // 官方 ChannelType：10005=直播、10006=应用、10007=论坛
        assert_eq!(ChannelKind::from(10005), ChannelKind::Live);
        assert_eq!(ChannelKind::from(10006), ChannelKind::Application);
        assert_eq!(ChannelKind::from(10007), ChannelKind::Post);
        assert_eq!(ChannelKind::from(10011), ChannelKind::Schedule);
        // 1/3 为保留值，不做语义映射
        assert!(matches!(ChannelKind::from(1), ChannelKind::Unknown(1)));
        assert!(matches!(ChannelKind::from(42), ChannelKind::Unknown(42)));
        // raw 往返
        for kind in [
            ChannelKind::Text,
            ChannelKind::Voice,
            ChannelKind::Category,
            ChannelKind::Live,
            ChannelKind::Application,
            ChannelKind::Post,
            ChannelKind::Schedule,
            ChannelKind::Unknown(77),
        ] {
            assert_eq!(ChannelKind::from(kind.raw()), kind);
        }
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

    // -- 频道管理（新增接口按官方文档的结构校验） -------------------------------

    #[test]
    fn create_channel_request_serializes_type_field() {
        let request = CreateChannelRequest::text("公告区")
            .with_sub_type(1)
            .with_position(3)
            .with_parent("0")
            .with_speak_permission(1);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["name"], serde_json::json!("公告区"));
        assert_eq!(json["type"], serde_json::json!(0));
        assert_eq!(json["sub_type"], serde_json::json!(1));
        assert_eq!(json["position"], serde_json::json!(3));
        assert_eq!(json["parent_id"], serde_json::json!("0"));
        assert_eq!(json["speak_permission"], serde_json::json!(1));
        assert!(json.get("application_id").is_none());

        let voice = CreateChannelRequest::voice("开黑房").with_sub_type(3);
        let json = serde_json::to_value(&voice).unwrap();
        assert_eq!(json["type"], serde_json::json!(2));
    }

    #[test]
    fn channel_permission_modify_merges_bitmap() {
        let request = ChannelPermissionModify::new()
            .grant_view()
            .grant_bits(4)
            .revoke_bits(2);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["add"], serde_json::json!("5"));
        assert_eq!(json["remove"], serde_json::json!("2"));
        assert!(!request.is_empty());
        assert!(ChannelPermissionModify::new().is_empty());
    }

    #[test]
    fn mute_requests_serialize_string_timestamps() {
        let json = serde_json::to_value(MuteRequest::seconds(120)).unwrap();
        assert_eq!(json["mute_seconds"], serde_json::json!("120"));
        assert!(json.get("mute_end_timestamp").is_none());

        let json = serde_json::to_value(MuteRequest::until(1641916800)).unwrap();
        assert_eq!(json["mute_end_timestamp"], serde_json::json!("1641916800"));

        let json = serde_json::to_value(MuteRequest::unmute()).unwrap();
        assert_eq!(json["mute_seconds"], serde_json::json!("0"));

        let multi = MuteMembersRequest::new(
            vec!["1201318637970874066".into(), "1201318637970874067".into()],
            MuteRequest::seconds(60),
        );
        let json = serde_json::to_value(&multi).unwrap();
        assert_eq!(json["mute_seconds"], serde_json::json!("60"));
        assert_eq!(json["user_ids"].as_array().unwrap().len(), 2);

        let response: MuteMembersResponse =
            serde_json::from_str(r#"{"user_ids":["1201318637970874066"]}"#).unwrap();
        assert_eq!(response.user_ids, vec!["1201318637970874066"]);
    }

    #[test]
    fn delete_member_request_omits_unset_fields() {
        let json = serde_json::to_value(DeleteMemberRequest::new()).unwrap();
        assert_eq!(json, serde_json::json!({}));

        let json = serde_json::to_value(
            DeleteMemberRequest::new()
                .blacklist(true)
                .delete_history_days(-1),
        )
        .unwrap();
        assert_eq!(json["add_blacklist"], serde_json::json!(true));
        assert_eq!(json["delete_history_msg_days"], serde_json::json!(-1));
    }

    #[test]
    fn message_setting_parses_documented_fields() {
        let body = r#"{
            "disable_create_dm": true,
            "disable_push_msg": false,
            "channel_ids": ["1146313", "2651849"],
            "channel_push_max_num": 12
        }"#;
        let setting: MessageSetting = serde_json::from_str(body).unwrap();
        assert!(setting.disable_create_dm);
        assert!(!setting.disable_push_msg);
        assert_eq!(setting.channel_ids, vec!["1146313", "2651849"]);
        assert_eq!(setting.channel_push_max_num, Some(12));
    }

    #[test]
    fn announce_request_message_and_recommend_shapes() {
        let message = CreateAnnounceRequest::message("123456", "xxxxxx");
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["channel_id"], serde_json::json!("123456"));
        assert_eq!(json["message_id"], serde_json::json!("xxxxxx"));
        assert_eq!(json["announces_type"], serde_json::json!(0));

        let recommend =
            CreateAnnounceRequest::recommend(vec![RecommendChannel::new("xxxx", "推荐语")]);
        let json = serde_json::to_value(&recommend).unwrap();
        assert!(json.get("message_id").is_none());
        assert_eq!(json["announces_type"], serde_json::json!(1));
        assert_eq!(json["recommend_channels"][0]["channel_id"], "xxxx");

        let response: Announces = serde_json::from_str(
            r#"{"guild_id":"1","channel_id":"123456","message_id":"xxxxxx","announces_type":0,"recommend_channels":[]}"#,
        )
        .unwrap();
        assert_eq!(response.message_id.as_deref(), Some("xxxxxx"));
    }

    #[test]
    fn patch_schedule_envelope_string_timestamps_and_empty_check() {
        assert!(PatchScheduleRequest::new().is_empty());
        let request = PatchScheduleRequest::new()
            .with_name("今晚八点上王者")
            .with_time_range(1642076453000, 1642083653000)
            .with_remind_type(RemindType::None);
        let envelope = PatchScheduleRequest::envelope(&request);
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            json["schedule"]["start_timestamp"],
            serde_json::json!("1642076453000")
        );
        assert_eq!(
            json["schedule"]["end_timestamp"],
            serde_json::json!("1642083653000")
        );
        assert_eq!(json["schedule"]["remind_type"], serde_json::json!("0"));
        assert!(!request.is_empty());
    }

    #[test]
    fn audio_control_shapes() {
        let play = AudioControl::play("http:/xxxxx.mp3", Some("简单爱-周杰伦".into()));
        let json = serde_json::to_value(&play).unwrap();
        assert_eq!(json["status"], serde_json::json!(0));
        assert_eq!(json["audio_url"], serde_json::json!("http:/xxxxx.mp3"));
        assert_eq!(json["text"], serde_json::json!("简单爱-周杰伦"));

        let stop = AudioControl::stop();
        let json = serde_json::to_value(&stop).unwrap();
        assert_eq!(json["status"], serde_json::json!(3));
        assert!(json.get("audio_url").is_none());
        assert!(json.get("text").is_none());
    }

    #[test]
    fn threads_page_and_get_thread_parsing() {
        // 官方示例：列表 is_finish=1，content 是 JSON 字符串
        let page: ThreadsPage = serde_json::from_str(
            r#"{"threads":[{"guild_id":"1","channel_id":"2","author_id":"3",
                "thread_info":{"thread_id":"t1","title":"帖子标题","content":"{}","date_time":"2022-02-21T12:08:57+08:00"}}],
                "is_finish":1}"#,
        )
        .unwrap();
        assert_eq!(page.threads.len(), 1);
        assert!(page.is_finish());
        assert_eq!(
            page.threads[0]
                .thread_info
                .as_ref()
                .unwrap()
                .thread_id
                .as_deref(),
            Some("t1")
        );

        #[derive(Deserialize)]
        struct Wrapper {
            thread: Thread,
        }
        let wrapper: Wrapper = serde_json::from_str(
            r#"{"thread":{"guild_id":"1","channel_id":"2","author_id":"3",
                "thread_info":{"thread_id":"t2","title":"标题"}}}"#,
        )
        .unwrap();
        assert_eq!(
            wrapper.thread.thread_info.unwrap().title.as_deref(),
            Some("标题")
        );

        let created: CreateThreadResponse =
            serde_json::from_str(r#"{"task_id":"1645413752912602306","create_time":"1645503180"}"#)
                .unwrap();
        assert_eq!(created.task_id, "1645413752912602306");
    }

    #[test]
    fn dms_session_and_request_shapes() {
        let request = CreateDmsRequest::new("123456", "112233");
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["recipient_id"], serde_json::json!("123456"));
        assert_eq!(json["source_guild_id"], serde_json::json!("112233"));

        let session: DmsSession = serde_json::from_str(
            r#"{"guild_id":"xxxxxx","channel_id":"xxxxxx","create_time":"1642545606"}"#,
        )
        .unwrap();
        assert_eq!(session.guild_id.as_deref(), Some("xxxxxx"));
    }

    #[test]
    fn api_permission_demand_shapes() {
        let request = CreateApiPermissionDemandRequest::new(
            "123456",
            ApiPermissionDemandIdentify::new("/guilds/{guild_id}", "GET"),
            "显示频道信息",
        );
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["api_identify"]["path"], "/guilds/{guild_id}");
        assert_eq!(json["api_identify"]["method"], "GET");

        let demand: ApiPermissionDemand = serde_json::from_str(
            r#"{"guild_id":"xxxxxx","channel_id":"123456",
                "api_identify":{"path":"/guilds/{guild_id}","method":"GET"},
                "title":"王者机器人申请授权频道信息接口权限","desc":"申请权限后才能使用"}"#,
        )
        .unwrap();
        assert_eq!(
            demand.title.as_deref(),
            Some("王者机器人申请授权频道信息接口权限")
        );
    }

    // -- 频道消息（新增字段 / 审核 / 表态） --------------------------------------

    #[test]
    fn message_request_serializes_new_message_types() {
        let embed = MessageEmbed::new("标题")
            .with_prompt("弹窗")
            .with_thumbnail("https://example.com/t.png")
            .with_field("字段");
        let request = ChannelMessageRequest::new()
            .with_embed(embed)
            .with_message_reference(MessageReference::new("MSG1").ignore_error(true))
            .with_msg_seq(2);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["embed"]["title"], serde_json::json!("标题"));
        assert_eq!(
            json["embed"]["thumbnail"]["url"],
            "https://example.com/t.png"
        );
        assert_eq!(
            json["embed"]["fields"][0]["name"],
            serde_json::json!("字段")
        );
        assert_eq!(
            json["message_reference"]["message_id"],
            serde_json::json!("MSG1")
        );
        assert_eq!(
            json["message_reference"]["ignore_get_message_error"],
            serde_json::json!(true)
        );
        assert_eq!(json["msg_seq"], serde_json::json!(2));
        assert!(request.has_sendable_content());

        let markdown = MessageMarkdown::template(
            1,
            vec![
                MessageMarkdownParam::single("title", "标题"),
                MessageMarkdownParam::new("list", vec!["a".into(), "b".into()]),
            ],
        );
        let json = serde_json::to_value(&markdown).unwrap();
        assert_eq!(json["template_id"], serde_json::json!(1));
        assert_eq!(json["params"][1]["values"], serde_json::json!(["a", "b"]));
        assert!(json.get("content").is_none());

        let ark = MessageArk {
            template_id: 1,
            kv: vec![MessageArkKv {
                key: "#DESC#".into(),
                value: None,
                obj: Some(vec![MessageArkObj {
                    obj_kv: vec![MessageArkObjKv::new("desc", "机器人订阅消息")],
                }]),
            }],
        };
        let json = serde_json::to_value(&ark).unwrap();
        assert_eq!(json["kv"][0]["obj"][0]["obj_kv"][0]["key"], "desc");
        assert!(json["kv"][0].get("value").is_none());
    }

    #[test]
    fn request_without_content_is_rejected_by_helper() {
        assert!(!ChannelMessageRequest::new().has_sendable_content());
        assert!(ChannelMessageRequest::text("hi").has_sendable_content());
        assert!(
            ChannelMessageRequest::new()
                .with_image("http://x")
                .has_sendable_content()
        );
    }

    #[test]
    fn multipart_form_builder_accepts_object_fields() {
        let request = ChannelMessageRequest::text("hello")
            .with_embed(MessageEmbed::new("标题"))
            .with_markdown(MessageMarkdown::native("# md"))
            .with_message_reference(MessageReference::new("m1"));
        // reqwest::multipart::Form 不暴露内容读取；此处验证构造不报错
        assert!(request.to_multipart_form().is_ok());
    }

    #[test]
    fn classify_detects_nested_audit_response() {
        // 官方文档形态：data.message_audit.audit_id
        let body = r#"{"code":304023,"message":"push message is waiting for audit now",
            "data":{"message_audit":{"audit_id":"ab9bd72f-19e8-4394-b09e-66caca0d64e4"}}}"#;
        let outcome = classify_send_body(body).unwrap();
        let audit = outcome.auditing().expect("应为审核结果");
        assert_eq!(
            audit.audit_id.as_deref(),
            Some("ab9bd72f-19e8-4394-b09e-66caca0d64e4")
        );
        assert_eq!(audit.code, Some(304023));
    }

    #[test]
    fn reaction_type_uses_documented_values() {
        // 官方 EmojiType：1=系统表情、2=emoji 表情
        assert_eq!(ReactionType::SystemFace as u8, 1);
        assert_eq!(ReactionType::Emoji as u8, 2);
        let json = serde_json::to_value(ReactionType::SystemFace).unwrap();
        assert_eq!(json, serde_json::json!(1));
        let parsed: ReactionType = serde_json::from_value(serde_json::json!(2)).unwrap();
        assert_eq!(parsed, ReactionType::Emoji);
    }

    #[test]
    fn reaction_users_page_accepts_users_and_legacy_data() {
        let official: ReactionUsersPage = serde_json::from_str(
            r#"{"users":[{"id":"1","username":"频道机器人","avatar":"http://a"}],
                "cookie":"1_2","is_end":false}"#,
        )
        .unwrap();
        assert_eq!(official.users.len(), 1);
        assert!(!official.is_end());

        // 兼容历史字段名 data
        let legacy: ReactionUsersPage =
            serde_json::from_str(r#"{"data":[{"id":"2"}],"is_end":true}"#).unwrap();
        assert_eq!(legacy.data().len(), 1);
        assert!(legacy.is_end());

        // 空响应场景
        let empty: ReactionUsersPage =
            serde_json::from_str(r#"{"users":[],"is_end":true}"#).unwrap();
        assert!(empty.users.is_empty());
    }

    #[test]
    fn channel_message_parses_extended_fields() {
        // 官方响应示例（节选） + 扩展字段
        let body = r#"{
            "id":"xxxxxx","channel_id":"1","guild_id":"2",
            "content":"<@!1234>hello world",
            "timestamp":"2021-05-13T14:45:45+08:00",
            "mention_everyone":false,
            "author":{"id":"3","username":"abc","bot":true},
            "attachments":[{"url":"https://example.com/a.png"}],
            "embeds":[{"title":"标题","prompt":"弹窗","fields":[{"name":"f"}]}],
            "mentions":[{"id":"4","username":"u"}],
            "ark":{"template_id":1,"kv":[{"key":"k","value":"v"}]},
            "message_reference":{"message_id":"m1","ignore_get_message_error":false},
            "seq_in_channel":"3"
        }"#;
        let message: ChannelMessage = serde_json::from_str(body).unwrap();
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(message.embeds.len(), 1);
        assert_eq!(message.mentions.len(), 1);
        assert_eq!(message.ark.as_ref().unwrap().template_id, 1);
        assert_eq!(message.message_reference.as_ref().unwrap().message_id, "m1");
        assert_eq!(message.mention_everyone, Some(false));
    }
}
