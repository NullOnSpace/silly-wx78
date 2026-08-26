pub mod auth;
pub mod direct_message;
pub mod guild;
pub mod handler;
pub mod share_link;
pub mod websocket;

use tracing::{debug, error};

pub(crate) const API_BASE_URL: &str = "https://api.sgroup.qq.com";

pub(crate) trait ApiError: Sized {
    fn from_api(msg: String) -> Self;
    fn from_json(e: serde_json::Error) -> Self;
}

pub(crate) async fn parse_response<T: serde::de::DeserializeOwned, E: ApiError>(
    response: reqwest::Response,
) -> Result<T, E> {
    let status = response.status();
    debug!(status = %status, "收到响应");

    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "未知错误".to_string());
        error!(status = %status, error = %error_text, "API请求失败");
        return Err(E::from_api(error_text));
    }

    let response_text = response
        .text()
        .await
        .unwrap_or_else(|_| "未知错误".to_string());

    match serde_json::from_str(&response_text) {
        Ok(data) => {
            debug!(response = %response_text, "成功解析响应JSON");
            Ok(data)
        }
        Err(e) => {
            error!(
                error = %e,
                response = %response_text,
                "JSON解析失败, 响应原生内容"
            );
            Err(E::from_json(e))
        }
    }
}

/// 解析"无内容"响应。
///
/// 实测（docs/API_VERIFICATION.md §五）：多数删除类操作返回 `HTTP 204` 或空 body / `{}`，
/// JSON 解析必须容忍空串与空对象。任何 2xx 状态码都视为成功，body 仅用于日志。
pub(crate) async fn parse_empty_response<E: ApiError>(
    response: reqwest::Response,
) -> Result<(), E> {
    let status = response.status();
    debug!(status = %status, "收到响应");

    if !status.is_success() {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "未知错误".to_string());
        error!(status = %status, error = %error_text, "API请求失败");
        return Err(E::from_api(error_text));
    }

    let text = response.text().await.unwrap_or_default();
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "{}" || trimmed == "null" {
        debug!("响应体为空/空对象，按成功处理");
    } else {
        // 2xx 但携带了未预期的非空内容：不影响结果，记录便于排查
        debug!(body = %trimmed, "忽略非空响应体");
    }
    Ok(())
}

/// 针对开放平台接口实测行为（docs/API_VERIFICATION.md）的反序列化工具集：
/// 平台大量使用"字符串形式的数字"（ID、权限位图、毫秒时间戳等），且部分字段
/// 在不同场景下可能是数字或字符串，这里提供宽松解析。
pub(crate) mod de {
    use serde::{Deserialize, Deserializer};

    /// 兼容"数字或十进制数字字符串"两种形态的 i64 反序列化。
    pub fn lenient_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Number(n) => n
                .as_i64()
                .ok_or_else(|| serde::de::Error::custom(format!("无法解析为 i64: {n}"))),
            serde_json::Value::String(s) => s
                .trim()
                .parse::<i64>()
                .map_err(|_| serde::de::Error::custom(format!("无法解析为 i64: {s}"))),
            other => Err(serde::de::Error::custom(format!(
                "期望数字或数字字符串, 实际: {other}"
            ))),
        }
    }

    /// [`lenient_i64`] 的可缺省版本：字段缺失或为 null 时返回 None。
    pub fn opt_lenient_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<serde_json::Value>::deserialize(deserializer)?;
        match value {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::Number(n)) => Ok(n.as_i64()),
            Some(serde_json::Value::String(s)) => Ok(s.trim().parse::<i64>().ok()),
            Some(other) => Err(serde::de::Error::custom(format!(
                "期望数字或数字字符串, 实际: {other}"
            ))),
        }
    }

    /// 兼容布尔与 0/1 的宽松反序列化。
    ///
    /// 实测（2026-08-26 真实请求）：`GET /guilds/{gid}/roles` 的 `hoist`
    /// 返回整数 `1`/`0` 而非布尔值，严格 bool 反序列化会直接失败。
    pub fn lenient_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Bool(b) => Ok(b),
            serde_json::Value::Number(n) => Ok(n.as_i64().unwrap_or_default() != 0),
            serde_json::Value::String(s) => match s.trim() {
                "1" | "true" => Ok(true),
                "0" | "false" | "" => Ok(false),
                other => Err(serde::de::Error::custom(format!(
                    "无法解析为布尔值: {other}"
                ))),
            },
            other => Err(serde::de::Error::custom(format!(
                "期望布尔或 0/1, 实际: {other}"
            ))),
        }
    }

    /// [`lenient_bool`] 的可缺省版本：字段缺失或为 null 时返回 None。
    pub fn opt_lenient_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<serde_json::Value>::deserialize(deserializer)?;
        match value {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::Bool(b)) => Ok(Some(b)),
            Some(serde_json::Value::Number(n)) => Ok(Some(n.as_i64().unwrap_or_default() != 0)),
            Some(serde_json::Value::String(s)) => match s.trim() {
                "1" | "true" => Ok(Some(true)),
                "0" | "false" | "" => Ok(Some(false)),
                other => Err(serde::de::Error::custom(format!(
                    "无法解析为布尔值: {other}"
                ))),
            },
            Some(other) => Err(serde::de::Error::custom(format!(
                "期望布尔或 0/1, 实际: {other}"
            ))),
        }
    }
}

pub use auth::{AuthClient, AuthConfig, AuthError};
pub use direct_message::{
    DirectMessageClient, DirectMessageError, DirectMessageRequest, DirectMessageResponse,
    MessageType,
};
pub use guild::{
    ApiPermissionEntry, ApiPermissionList, ChannelInfo, ChannelKind, ChannelMessage,
    ChannelMessageRequest, ChannelMessageSendOutcome, ChannelPermissions, CreateRoleRequest, Guild,
    GuildClient, GuildError, GuildRoles, Member, MessageAuditInfo, MessageSetting,
    ModifyRoleRequest, PinMessages, ReactionType, ReactionUser, ReactionUsersPage, RemindType,
    Role, RoleCreated, RoleMembersPage, Schedule, ScheduleRequest, UserInfo,
};
pub use handler::DefaultMessageHandler;
pub use share_link::{ShareLinkClient, ShareLinkError, ShareLinkRequest, ShareLinkResponse};
pub use tokio_util::sync::CancellationToken;
pub use websocket::{
    ChannelEvent, ChannelEventType, ChannelMessageEvent, DirectMessageEvent, DisconnectReason,
    GroupMessageEvent, GuildEvent, GuildEventType, HandlerFuture, MessageAuthor, MessageEvent,
    MessageMember, MessageScene, OpenIdAuthor, WebSocketClient, WebSocketConfig, WebSocketError,
    WebSocketHandler, intents,
};
