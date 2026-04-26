pub mod auth;
pub mod direct_message;
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
        let error_text = response.text().await.unwrap_or_else(|_| "未知错误".to_string());
        error!(status = %status, error = %error_text, "API请求失败");
        return Err(E::from_api(error_text));
    }

    let response_text = response.text().await.unwrap_or_else(|_| "未知错误".to_string());

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

pub use auth::{AuthClient, AuthConfig};
pub use direct_message::{
    DirectMessageClient, DirectMessageError, DirectMessageRequest, DirectMessageResponse,
    MessageType,
};
pub use handler::DefaultMessageHandler;
pub use share_link::{ShareLinkClient, ShareLinkError, ShareLinkRequest, ShareLinkResponse};
pub use tokio_util::sync::CancellationToken;
pub use websocket::{
    DisconnectReason, MessageAuthor, MessageEvent, MessageScene, WebSocketClient, WebSocketConfig,
    WebSocketError, WebSocketHandler,
};
