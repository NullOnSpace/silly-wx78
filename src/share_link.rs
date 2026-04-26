use crate::auth::AuthClient;
use crate::{API_BASE_URL, ApiError, parse_response};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, info};

#[derive(Error, Debug)]
pub enum ShareLinkError {
    #[error("HTTP请求错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("认证错误: {0}")]
    Auth(#[from] crate::auth::AuthError),

    #[error("API错误: {0}")]
    Api(String),

    #[error("JSON解析错误: {0}")]
    Json(#[from] serde_json::Error),
}

impl ApiError for ShareLinkError {
    fn from_api(msg: String) -> Self {
        Self::Api(msg)
    }

    fn from_json(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

#[derive(Debug, Serialize)]
pub struct ShareLinkRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
}

impl ShareLinkRequest {
    pub fn new() -> Self {
        Self {
            callback_data: None,
        }
    }

    pub fn with_callback_data(callback_data: String) -> Self {
        Self {
            callback_data: Some(callback_data),
        }
    }
}

impl Default for ShareLinkRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
pub struct ShareLinkResponse {
    pub url: String,
}

pub struct ShareLinkClient {
    http_client: Arc<Client>,
    auth_client: Arc<AuthClient>,
}

impl ShareLinkClient {
    pub fn new(http_client: Arc<Client>, auth_client: Arc<AuthClient>) -> Self {
        Self {
            http_client,
            auth_client,
        }
    }

    pub async fn generate_url_link(
        &self,
        request: ShareLinkRequest,
    ) -> Result<ShareLinkResponse, ShareLinkError> {
        let url = format!("{}/v2/generate_url_link", API_BASE_URL);
        let authorization = self.auth_client.get_authorization().await?;

        info!(
            callback_data = ?request.callback_data,
            "生成机器人分享链接"
        );
        debug!(url = %url, "请求URL");

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", &authorization)
            .json(&request)
            .send()
            .await?;

        let share_link_response: ShareLinkResponse =
            parse_response::<_, ShareLinkError>(response).await?;

        info!(
            url = %share_link_response.url,
            "分享链接生成成功"
        );

        Ok(share_link_response)
    }

    pub async fn generate_share_link(&self) -> Result<String, ShareLinkError> {
        let request = ShareLinkRequest::new();
        let response = self.generate_url_link(request).await?;
        Ok(response.url)
    }

    pub async fn generate_share_link_with_callback(
        &self,
        callback_data: String,
    ) -> Result<String, ShareLinkError> {
        let request = ShareLinkRequest::with_callback_data(callback_data);
        let response = self.generate_url_link(request).await?;
        Ok(response.url)
    }
}
