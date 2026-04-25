use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

const AUTH_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("环境变量错误: {0}")]
    EnvVar(#[from] env::VarError),

    #[error("HTTP请求错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API错误: {0}")]
    Api(String),

    #[error("Token已过期")]
    TokenExpired,
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub app_id: String,
    pub client_secret: String,
}

impl AuthConfig {
    pub fn from_env() -> Result<Self, AuthError> {
        debug!("从环境变量加载配置");
        let app_id = env::var("QQ_BOT_APP_ID")?;
        let client_secret = env::var("QQ_BOT_CLIENT_SECRET")?;
        info!(app_id = %app_id, "成功加载配置");
        Ok(Self {
            app_id,
            client_secret,
        })
    }
}

#[derive(Debug, Serialize)]
struct TokenRequest {
    app_id: String,
    client_secret: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(deserialize_with = "deserialize_string_to_i64")]
    expires_in: i64,
}

fn deserialize_string_to_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = String::deserialize(deserializer)?;
    s.parse::<i64>().map_err(serde::de::Error::custom)
}

struct CachedToken {
    access_token: Arc<str>,
    expires_at: Instant,
}

impl CachedToken {
    fn new(access_token: Arc<str>, expires_in: i64) -> Self {
        let expires_at = Instant::now() + std::time::Duration::from_secs((expires_in - 60) as u64);
        Self {
            access_token,
            expires_at,
        }
    }

    fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

pub struct AuthClient {
    config: AuthConfig,
    http_client: Arc<Client>,
    cached_token: RwLock<Option<CachedToken>>,
}

impl AuthClient {
    pub fn new(config: AuthConfig, http_client: Arc<Client>) -> Self {
        Self {
            config,
            http_client,
            cached_token: RwLock::new(None),
        }
    }

    pub async fn get_access_token(&self) -> Result<Arc<str>, AuthError> {
        debug!("尝试获取access_token");
        let cached = self.cached_token.read().await;
        if let Some(token) = cached.as_ref() {
            if !token.is_expired() {
                debug!("使用缓存的access_token");
                return Ok(Arc::clone(&token.access_token));
            } else {
                warn!("缓存的access_token已过期，需要刷新");
            }
        } else {
            debug!("缓存中没有access_token，需要获取新的token");
        }
        drop(cached);

        self.refresh_token().await
    }

    async fn refresh_token(&self) -> Result<Arc<str>, AuthError> {
        info!("开始刷新access_token");
        let request = TokenRequest {
            app_id: self.config.app_id.clone(),
            client_secret: self.config.client_secret.clone(),
        };

        debug!(url = %AUTH_URL, "发送获取token请求");
        let response = self
            .http_client
            .post(AUTH_URL)
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        debug!(status = %status, "收到响应");

        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_else(|_| "未知错误".to_string());
            error!(status = %status, error = %error_text, "获取token失败");
            return Err(AuthError::Api(error_text));
        }

        let response_text = response.text().await.unwrap_or_else(|_| "未知错误".to_string());

        let token_response: TokenResponse = match serde_json::from_str(&response_text) {
            Ok(token) => {
                debug!(response = %response_text, "成功解析响应JSON");
                token
            }
            Err(e) => {
                error!(
                    error = %e,
                    response = %response_text,
                    "JSON解析失败, 响应原生内容"
                );
                return Err(AuthError::Api(format!("JSON解析失败: {}", e)));
            }
        };

        info!(
            expires_in = token_response.expires_in,
            "成功获取新的access_token"
        );

        let access_token: Arc<str> = Arc::from(token_response.access_token);
        let cached_token = CachedToken::new(Arc::clone(&access_token), token_response.expires_in);
        debug!("缓存新的access_token");

        let mut cache = self.cached_token.write().await;
        *cache = Some(cached_token);

        Ok(access_token)
    }

    pub fn authorization_header(token: &str) -> String {
        format!("QQBot {}", token)
    }
}