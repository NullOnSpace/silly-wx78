use crate::{parse_response, ApiError, API_BASE_URL};
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

    #[error("API错误: {0}")]
    Api(String),

    #[error("JSON解析错误: {0}")]
    Json(#[from] serde_json::Error),
}

impl ApiError for DirectMessageError {
    fn from_api(msg: String) -> Self {
        Self::Api(msg)
    }

    fn from_json(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

#[derive(Debug, Clone, Copy, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum MessageType {
    Text = 0,
    Ark = 3,
    Embed = 4,
    Media = 7,
}

#[derive(Debug, Serialize)]
pub struct DirectMessageRequest {
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
        }
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

#[derive(Debug, Deserialize)]
pub struct DirectMessageEvent {
    pub id: String,
    pub author: Author,
    pub content: String,
    pub timestamp: String,
    pub guild_id: String,
}

#[derive(Debug, Deserialize)]
pub struct Author {
    pub id: String,
    pub username: String,
    pub avatar: Option<String>,
}

pub struct DirectMessageClient {
    http_client: Arc<Client>,
    authorization: Arc<str>,
}

impl DirectMessageClient {
    pub fn new(http_client: Arc<Client>, access_token: Arc<str>) -> Self {
        let authorization = Arc::from(format!("QQBot {}", access_token));
        Self {
            http_client,
            authorization,
        }
    }

    pub async fn send_message(
        &self,
        openid: &str,
        message: DirectMessageRequest,
    ) -> Result<DirectMessageResponse, DirectMessageError> {
        let url = format!("{}/v2/users/{}/messages", API_BASE_URL, openid);

        info!(openid = %openid, message = ?message, "发送单聊消息");
        debug!(url = %url, "请求URL");

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", &*self.authorization)
            .json(&message)
            .send()
            .await?;

        let message_response: DirectMessageResponse = parse_response::<_, DirectMessageError>(response).await?;

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
