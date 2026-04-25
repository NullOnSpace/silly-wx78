use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, error, info};

const API_BASE_URL: &str = "https://api.sgroup.qq.com";

#[derive(Error, Debug)]
pub enum ShareLinkError {
    #[error("HTTP请求错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API错误: {0}")]
    Api(String),

    #[error("JSON解析错误: {0}")]
    Json(#[from] serde_json::Error),
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
    access_token: Arc<str>,
}

impl ShareLinkClient {
    pub fn new(http_client: Arc<Client>, access_token: Arc<str>) -> Self {
        Self {
            http_client,
            access_token,
        }
    }

    pub async fn generate_url_link(
        &self,
        request: ShareLinkRequest,
    ) -> Result<ShareLinkResponse, ShareLinkError> {
        let url = format!("{}/v2/generate_url_link", API_BASE_URL);

        info!(
            callback_data = ?request.callback_data,
            "生成机器人分享链接"
        );
        debug!(url = %url, "请求URL");

        let response = self
            .http_client
            .post(&url)
            .header(
                "Authorization",
                format!("QQBot {}", self.access_token),
            )
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        debug!(status = %status, "收到响应");

        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_else(|_| "未知错误".to_string());
            error!(status = %status, error = %error_text, "生成分享链接失败");
            return Err(ShareLinkError::Api(error_text));
        }

        let response_text = response.text().await.unwrap_or_else(|_| "未知错误".to_string());

        let share_link_response: ShareLinkResponse = match serde_json::from_str(&response_text) {
            Ok(link) => {
                debug!(response = %response_text, "成功解析响应JSON");
                link
            }
            Err(e) => {
                error!(
                    error = %e,
                    response = %response_text,
                    "JSON解析失败, 响应原生内容"
                );
                return Err(ShareLinkError::Json(e));
            }
        };

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