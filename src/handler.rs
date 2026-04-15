use crate::websocket::{EventHandler, MessageEvent};
use crate::DirectMessageClient;
use async_trait::async_trait;
use tracing::{error, info};

pub struct DefaultMessageHandler;

#[async_trait]
impl EventHandler for DefaultMessageHandler {
    async fn on_message(&self, message: MessageEvent, token: &str) {
        info!("处理消息: {}", message.content);
        
        let openid = message.author.user_openid.clone().unwrap_or_else(|| message.author.id.clone());
        let reply_content = format!("Msg Got: {}", message.content);
        
        let client = DirectMessageClient::new(token.to_string());
        
        match client.reply_message(&openid, message.id.clone(), reply_content.clone(), None).await {
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
