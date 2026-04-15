use crate::websocket::{MessageEvent, WebSocketClient, WebSocketHandler};
use crate::DirectMessageClient;
use async_trait::async_trait;
use std::sync::Arc;
use tracing::{error, info};

pub struct DefaultMessageHandler;

#[async_trait]
impl WebSocketHandler for DefaultMessageHandler {
    async fn on_message(&self, message: MessageEvent, client: Arc<WebSocketClient>) {
        info!("处理消息: {}", message.content);
        
        let openid = message.author.user_openid.clone().unwrap_or_else(|| message.author.id.clone());
        let reply_content = format!("Msg Got: {}", message.content);
        
        let dm_client = DirectMessageClient::new(client.token().to_string());
        
        match dm_client.reply_message(&openid, message.id.clone(), reply_content.clone(), None).await {
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
    }
}
