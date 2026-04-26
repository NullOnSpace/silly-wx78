use crate::DirectMessageClient;
use crate::websocket::{MessageEvent, WebSocketClient, WebSocketHandler};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{error, info};

pub struct DefaultMessageHandler {
    dm_client: Arc<DirectMessageClient>,
}

impl DefaultMessageHandler {
    pub fn new(dm_client: DirectMessageClient) -> Self {
        Self {
            dm_client: Arc::new(dm_client),
        }
    }
}

impl WebSocketHandler for DefaultMessageHandler {
    fn on_message(
        &self,
        message: MessageEvent,
        _client: Arc<WebSocketClient>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let dm_client = Arc::clone(&self.dm_client);
        Box::pin(async move {
            info!("处理消息: {}", message.content);

            let openid = message
                .author
                .user_openid
                .as_deref()
                .unwrap_or(&message.author.id);
            let reply_content = format!("Msg Got: {}", &message.content);

            match dm_client
                .reply_message(openid, message.id.clone(), reply_content.clone(), None)
                .await
            {
                Ok(response) => {
                    info!(
                        message_id = %response.id,
                        openid = %openid,
                        content = %reply_content,
                        "回复消息成功"
                    );
                }
                Err(e) => {
                    error!(error = %e, openid = %openid, "回复消息失败");
                }
            }
        })
    }
}
