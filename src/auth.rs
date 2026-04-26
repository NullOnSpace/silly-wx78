use crate::{ApiError, parse_response};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

const AUTH_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("环境变量错误: {0}")]
    EnvVar(#[from] env::VarError),

    #[error("HTTP请求错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API错误: {0}")]
    Api(String),

    #[error("JSON解析错误: {0}")]
    Json(#[from] serde_json::Error),
}

impl ApiError for AuthError {
    fn from_api(msg: String) -> Self {
        Self::Api(msg)
    }

    fn from_json(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub app_id: Arc<str>,
    pub client_secret: Arc<str>,
}

impl AuthConfig {
    pub fn from_env() -> Result<Self, AuthError> {
        debug!("从环境变量加载配置");
        let app_id = Arc::from(env::var("QQ_BOT_APP_ID")?) as Arc<str>;
        let client_secret = Arc::from(env::var("QQ_BOT_CLIENT_SECRET")?) as Arc<str>;
        info!(app_id = %app_id, "成功加载配置");
        Ok(Self {
            app_id,
            client_secret,
        })
    }
}

#[derive(Debug, Serialize)]
struct TokenRequest<'a> {
    app_id: &'a str,
    client_secret: &'a str,
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
        let mut cache = self.cached_token.write().await;
        if let Some(token) = cache.as_ref()
            && !token.is_expired()
        {
            debug!("其他调用者已刷新token，使用缓存");
            return Ok(Arc::clone(&token.access_token));
        }

        info!("开始刷新access_token");
        let request = TokenRequest {
            app_id: &self.config.app_id,
            client_secret: &self.config.client_secret,
        };

        debug!(url = %AUTH_URL, "发送获取token请求");
        let response = self
            .http_client
            .post(AUTH_URL)
            .json(&request)
            .send()
            .await?;

        let token_response: TokenResponse = parse_response::<_, AuthError>(response).await?;

        info!(
            expires_in = token_response.expires_in,
            "成功获取新的access_token"
        );

        let access_token: Arc<str> = Arc::from(token_response.access_token);
        let cached_token = CachedToken::new(Arc::clone(&access_token), token_response.expires_in);
        debug!("缓存新的access_token");

        *cache = Some(cached_token);

        Ok(access_token)
    }

    pub async fn get_authorization(&self) -> Result<String, AuthError> {
        let token = self.get_access_token().await?;
        Ok(format!("QQBot {}", token))
    }
}
