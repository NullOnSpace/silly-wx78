use crate::websocket::{MessageEvent, WebSocketClient, WebSocketHandler};
use crate::DirectMessageClient;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{error, info};

pub struct DefaultMessageHandler;

impl WebSocketHandler for DefaultMessageHandler {
    fn on_message(&self, message: MessageEvent, client: Arc<WebSocketClient>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            info!("处理消息: {}", message.content);

            let openid = message.author.user_openid.as_deref().unwrap_or(&message.author.id);
            let reply_content = format!("Msg Got: {}", &message.content);

            let dm_client = DirectMessageClient::new(
                Arc::clone(client.http_client()),
                Arc::clone(client.token()),
            );

            match dm_client.reply_message(openid, message.id.clone(), reply_content.clone(), None).await {
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