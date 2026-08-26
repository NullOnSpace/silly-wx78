use crate::auth::AuthClient;
use crate::{API_BASE_URL, ApiError, parse_response};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, info};

#[derive(Error, Debug)]
pub enum DirectMessageError {
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

impl ApiError for DirectMessageError {
    fn from_api(msg: String) -> Self {
        Self::Api(msg)
    }

    fn from_json(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum MessageType {
    Text = 0,
    /// Markdown 消息（单聊/群聊发送，详见 docs/DIRECT_MESSAGE.md）
    Markdown = 2,
    Ark = 3,
    Embed = 4,
    Media = 7,
}

/// 模板 markdown 的变量填充项：模版内 `{{.key}}` 与填充值的 kv 映射。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MarkdownParam {
    pub key: String,
    pub values: Vec<String>,
}

impl MarkdownParam {
    pub fn new(key: impl Into<String>, values: Vec<String>) -> Self {
        Self {
            key: key.into(),
            values,
        }
    }

    /// 单值便捷构造（平台取 values[0]）
    pub fn single(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new(key, vec![value.into()])
    }
}

/// Markdown 消息载荷：请求体中独立的 `markdown` 字段（msg_type = 2 时携带）。
///
/// 协议（官方文档《Markdown 消息》数据结构与协议）：
///
/// | 属性 | 类型 | 必填 | 说明 |
/// |------|------|------|------|
/// | content | string | 否 | 自定义 markdown 文本内容 |
/// | custom_template_id | string | 否 | markdown 模版id，申请模版后获得 |
/// | params | Array | 否 | {key: xxx, values: xxx}，模版内变量与填充值的kv映射 |
///
/// `content` 与 `custom_template_id` **二选一**，本库在构造与发送时都会校验。
///
/// 支持语法（节选）：`#`~`###` 标题、`**加粗**`、`__下划线加粗__`、`*斜体*`、
/// `***粗斜体***`、`~~删除线~~`、`[文字](url)` 链接、
/// `![说明 #宽px #高px](url)` 图片（公网可访问 url，平台会转存）、
/// 有序/无序列表及嵌套、`>` 块引用、`***` 分割线。
#[derive(Debug, Clone, Default, Serialize)]
pub struct MarkdownPayload {
    /// 自定义 markdown 文本内容（与模板二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// markdown 模版 id（与自定义文本二选一）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_template_id: Option<String>,
    /// 模版变量与填充值的 kv 映射
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Vec<MarkdownParam>>,
}

impl MarkdownPayload {
    /// 原生自定义 markdown 文本
    pub fn native(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            ..Self::default()
        }
    }

    /// 模板 markdown（模板在开放平台管理端申请后获得 id）
    pub fn template(template_id: impl Into<String>, params: Vec<MarkdownParam>) -> Self {
        Self {
            content: None,
            custom_template_id: Some(template_id.into()),
            params: Some(params),
        }
    }

    /// 校验载荷合法性：content 与 custom_template_id 二选一且必居其一
    pub fn validate(&self) -> Result<(), DirectMessageError> {
        match (&self.content, &self.custom_template_id) {
            (Some(_), Some(_)) => Err(DirectMessageError::InvalidRequest(
                "markdown 载荷的 content 与 custom_template_id 只能二选一".to_string(),
            )),
            (None, None) => Err(DirectMessageError::InvalidRequest(
                "markdown 载荷必须提供 content 或 custom_template_id 之一".to_string(),
            )),
            (Some(_), None) => Ok(()),
            (None, Some(_)) => Ok(()),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DirectMessageRequest {
    /// 文本内容（msg_type=Text 时使用；markdown 请求不携带）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub msg_type: MessageType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg_seq: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_wakeup: Option<bool>,
    /// msg_type = Markdown(2) 时携带的 markdown 载荷
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownPayload>,
}

impl DirectMessageRequest {
    pub fn text(content: String) -> Self {
        Self {
            content: Some(content),
            msg_type: MessageType::Text,
            event_id: None,
            msg_id: None,
            msg_seq: None,
            is_wakeup: None,
            markdown: None,
        }
    }

    pub fn reply(msg_id: String, content: String, msg_seq: Option<i32>) -> Self {
        Self {
            content: Some(content),
            msg_type: MessageType::Text,
            event_id: None,
            msg_id: Some(msg_id),
            msg_seq,
            is_wakeup: None,
            markdown: None,
        }
    }

    /// 发送原生自定义 markdown 消息（主动消息）。
    ///
    /// 单聊场景建议使用 [`DirectMessageRequest::reply_markdown`] 被动回复：
    /// 主动消息受审核/配额限制，被动回复的 msg_id 有效期约 5 分钟、最多回复 5 次。
    pub fn markdown(content: impl Into<String>) -> Self {
        Self {
            content: None,
            msg_type: MessageType::Markdown,
            event_id: None,
            msg_id: None,
            msg_seq: None,
            is_wakeup: None,
            markdown: Some(MarkdownPayload::native(content)),
        }
    }

    /// 发送模板 markdown 消息（主动消息）。模板与参数在开放平台管理端配置。
    pub fn markdown_template(template_id: impl Into<String>, params: Vec<MarkdownParam>) -> Self {
        Self {
            content: None,
            msg_type: MessageType::Markdown,
            event_id: None,
            msg_id: None,
            msg_seq: None,
            is_wakeup: None,
            markdown: Some(MarkdownPayload::template(template_id, params)),
        }
    }

    /// 被动回复一条原生 markdown 消息。msg_seq 从 1 开始，同一 msg_id 最多回复 5 次。
    pub fn reply_markdown(
        msg_id: impl Into<String>,
        content: impl Into<String>,
        msg_seq: i32,
    ) -> Self {
        Self {
            content: None,
            msg_type: MessageType::Markdown,
            event_id: None,
            msg_id: Some(msg_id.into()),
            msg_seq: Some(msg_seq),
            is_wakeup: None,
            markdown: Some(MarkdownPayload::native(content)),
        }
    }

    /// 在现有请求上附加/替换 markdown 载荷（同时把 msg_type 置为 Markdown，
    /// 并清空互斥的 content 字段）
    pub fn with_markdown(mut self, payload: MarkdownPayload) -> Self {
        self.content = None;
        self.msg_type = MessageType::Markdown;
        self.markdown = Some(payload);
        self
    }

    fn validate(&self) -> Result<(), DirectMessageError> {
        if self.msg_type == MessageType::Markdown {
            let Some(payload) = &self.markdown else {
                return Err(DirectMessageError::InvalidRequest(
                    "msg_type=Markdown 时必须携带 markdown 载荷".to_string(),
                ));
            };
            payload.validate()?;
        } else if self.markdown.is_some() {
            return Err(DirectMessageError::InvalidRequest(
                "仅 msg_type=Markdown 支持 markdown 载荷".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct DirectMessageResponse {
    pub id: String,
    pub timestamp: String,
    #[serde(rename = "ext_info")]
    pub ext_info: Option<ExtInfo>,
}

#[derive(Debug, Deserialize)]
pub struct ExtInfo {
    #[serde(rename = "ref_idx")]
    pub ref_idx: Option<String>,
}

pub struct DirectMessageClient {
    http_client: Arc<Client>,
    auth_client: Arc<AuthClient>,
}

impl DirectMessageClient {
    pub fn new(http_client: Arc<Client>, auth_client: Arc<AuthClient>) -> Self {
        Self {
            http_client,
            auth_client,
        }
    }

    pub async fn send_message(
        &self,
        openid: &str,
        message: DirectMessageRequest,
    ) -> Result<DirectMessageResponse, DirectMessageError> {
        // 发送前校验（markdown 载荷与 msg_type 的匹配性、content/模板二选一）
        message.validate()?;

        let url = format!("{}/v2/users/{}/messages", API_BASE_URL, openid);
        let authorization = self.auth_client.get_authorization().await?;

        info!(openid = %openid, message = ?message, "发送单聊消息");
        debug!(url = %url, "请求URL");

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", &authorization)
            .json(&message)
            .send()
            .await?;

        let message_response: DirectMessageResponse =
            parse_response::<_, DirectMessageError>(response).await?;

        info!(
            message_id = %message_response.id,
            timestamp = message_response.timestamp,
            "消息发送成功"
        );

        Ok(message_response)
    }

    pub async fn send_text(
        &self,
        openid: &str,
        content: String,
    ) -> Result<DirectMessageResponse, DirectMessageError> {
        let message = DirectMessageRequest::text(content);
        self.send_message(openid, message).await
    }

    /// 发送原生自定义 markdown 消息（主动）。
    ///
    /// ⚠️ 单聊主动消息受审核/配额限制，常规业务请使用 [`Self::reply_markdown`] 被动回复。
    pub async fn send_markdown(
        &self,
        openid: &str,
        content: impl Into<String>,
    ) -> Result<DirectMessageResponse, DirectMessageError> {
        let message = DirectMessageRequest::markdown(content);
        self.send_message(openid, message).await
    }

    /// 发送模板 markdown 消息（主动）。模板 id 与变量在开放平台管理端配置。
    pub async fn send_markdown_template(
        &self,
        openid: &str,
        template_id: impl Into<String>,
        params: Vec<MarkdownParam>,
    ) -> Result<DirectMessageResponse, DirectMessageError> {
        let message = DirectMessageRequest::markdown_template(template_id, params);
        self.send_message(openid, message).await
    }

    /// 被动回复一条原生 markdown 消息。msg_seq 从 1 开始，同一 msg_id 最多回复 5 次。
    pub async fn reply_markdown(
        &self,
        openid: &str,
        msg_id: impl Into<String>,
        content: impl Into<String>,
        msg_seq: i32,
    ) -> Result<DirectMessageResponse, DirectMessageError> {
        let message = DirectMessageRequest::reply_markdown(msg_id, content, msg_seq);
        self.send_message(openid, message).await
    }

    pub async fn reply_message(
        &self,
        openid: &str,
        msg_id: String,
        content: String,
        msg_seq: Option<i32>,
    ) -> Result<DirectMessageResponse, DirectMessageError> {
        let message = DirectMessageRequest::reply(msg_id, content, msg_seq);
        self.send_message(openid, message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_markdown_serializes_markdown_field_without_content() {
        // 官方协议：markdown 是独立字段，msg_type=2；原生 markdown 不带模板参数
        let request = DirectMessageRequest::reply_markdown("MSG_ID_1", "# 标题\n**加粗**", 1);
        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["msg_type"], serde_json::json!(2));
        assert_eq!(
            json["markdown"]["content"],
            serde_json::json!("# 标题\n**加粗**")
        );
        assert!(
            json.get("markdown")
                .unwrap()
                .get("custom_template_id")
                .is_none()
        );
        assert!(json.get("markdown").unwrap().get("params").is_none());
        // content 字段不应出现（与 markdown 载荷互斥）
        assert!(json.get("content").is_none());
        assert_eq!(json["msg_id"], serde_json::json!("MSG_ID_1"));
        assert_eq!(json["msg_seq"], serde_json::json!(1));
    }

    #[test]
    fn template_markdown_serializes_template_and_params() {
        // 官方文档模版示例的形状：custom_template_id + params[{key, values}]
        let request = DirectMessageRequest::markdown_template(
            "101993071_1658748972",
            vec![
                MarkdownParam::single("title", "标题"),
                MarkdownParam::new("link", vec!["https://www.qq.com".to_string()]),
            ],
        );
        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["msg_type"], serde_json::json!(2));
        assert_eq!(
            json["markdown"]["custom_template_id"],
            serde_json::json!("101993071_1658748972")
        );
        let params = json["markdown"]["params"].as_array().unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0]["key"], serde_json::json!("title"));
        assert_eq!(params[0]["values"], serde_json::json!(["标题"]));
        assert!(json["markdown"].get("content").is_none());
        assert!(json.get("content").is_none());
    }

    #[test]
    fn text_request_keeps_legacy_shape() {
        // 历史行为回归：文本请求不携带 markdown 字段
        let json = serde_json::to_value(DirectMessageRequest::text("hi".into())).unwrap();
        assert_eq!(json["msg_type"], serde_json::json!(0));
        assert_eq!(json["content"], serde_json::json!("hi"));
        assert!(json.get("markdown").is_none());

        let json =
            serde_json::to_value(DirectMessageRequest::reply("m1".into(), "hi".into(), None))
                .unwrap();
        assert!(json.get("markdown").is_none());
        assert!(json.get("msg_seq").is_none());
    }

    #[test]
    fn with_markdown_overrides_content_and_type() {
        let request = DirectMessageRequest::text("将被清空".into())
            .with_markdown(MarkdownPayload::native("# md"));
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("content").is_none());
        assert_eq!(json["msg_type"], serde_json::json!(2));
        assert_eq!(json["markdown"]["content"], serde_json::json!("# md"));
    }

    #[test]
    fn payload_validate_enforces_xor() {
        assert!(
            MarkdownPayload::native("# ok".to_string())
                .validate()
                .is_ok()
        );

        let template =
            MarkdownPayload::template("t1".to_string(), vec![MarkdownParam::single("k", "v")]);
        assert!(template.validate().is_ok());

        // 同时设置两者 → 拒绝
        let both = MarkdownPayload {
            content: Some("# a".into()),
            custom_template_id: Some("t1".into()),
            params: None,
        };
        assert!(both.validate().is_err());

        // 两者皆空 → 拒绝
        assert!(MarkdownPayload::default().validate().is_err());
    }

    #[test]
    fn request_validate_matches_type_with_payload() {
        // msg_type=Markdown 但缺载荷 → 拒绝
        let broken = DirectMessageRequest {
            content: None,
            msg_type: MessageType::Markdown,
            event_id: None,
            msg_id: Some("m1".into()),
            msg_seq: Some(1),
            is_wakeup: None,
            markdown: None,
        };
        assert!(matches!(
            broken.validate(),
            Err(DirectMessageError::InvalidRequest(_))
        ));

        // 非 markdown 类型却带载荷 → 拒绝
        let mut wrong = DirectMessageRequest::text("hi".into());
        wrong.markdown = Some(MarkdownPayload::native("# x"));
        assert!(matches!(
            wrong.validate(),
            Err(DirectMessageError::InvalidRequest(_))
        ));
    }
}
