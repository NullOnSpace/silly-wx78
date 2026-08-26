use crate::DirectMessageClient;
use crate::guild::GuildClient;
use crate::websocket::{ChannelMessageEvent, MessageEvent, WebSocketClient, WebSocketHandler};
use std::pin::Pin;
use std::sync::Arc;
use tracing::{error, info};

/// 默认演示处理器：收到消息后原样回显。
///
/// - 单聊(C2C)文本：通过 [`DirectMessageClient`] 回复（历史行为，保持不变）；
/// - 频道消息：若配置了 [`GuildClient`] 则通过被动回复（msg_id + msg_seq）在原子频道回显，
///   未配置时仅记录日志。
///
/// 注意平台频控：被动回复的 msg_id 有效期约 5 分钟、同一 msg_id 最多回复 5 次。
pub struct DefaultMessageHandler {
    dm_client: Arc<DirectMessageClient>,
    guild_client: Option<Arc<GuildClient>>,
}

impl DefaultMessageHandler {
    pub fn new(dm_client: DirectMessageClient) -> Self {
        Self {
            dm_client: Arc::new(dm_client),
            guild_client: None,
        }
    }

    /// 配置频道客户端以启用频道消息回显
    pub fn with_guild_client(mut self, guild_client: GuildClient) -> Self {
        self.guild_client = Some(Arc::new(guild_client));
        self
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
            info!("处理单聊消息: {}", message.content);

            let openid = message
                .author
                .user_openid
                .as_deref()
                .unwrap_or(&message.author.id);
            let reply_content = format!("Msg Got: {}", message.content);

            match dm_client
                .reply_message(openid, message.id.clone(), reply_content.clone(), None)
                .await
            {
                Ok(response) => {
                    info!(
                        message_id = %response.id,
                        openid = %openid,
                        content = %reply_content,
                        "回复单聊消息成功"
                    );
                }
                Err(e) => {
                    error!(error = %e, openid = %openid, "回复单聊消息失败");
                }
            }
        })
    }

    fn on_channel_message(
        &self,
        event: ChannelMessageEvent,
        _client: Arc<WebSocketClient>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let guild_client = self.guild_client.clone();
        Box::pin(async move {
            let Some(guild_client) = guild_client else {
                info!(
                    channel_id = %event.channel_id,
                    content = %event.content,
                    "收到频道消息（未配置 GuildClient，跳过回显）"
                );
                return;
            };

            let reply_content = format!("Msg Got: {}", event.content);
            // 被动回复带 msg_seq=1；如需对同一消息多次回复，递增 msg_seq（上限 5 次）
            match guild_client
                .reply_text(
                    &event.channel_id,
                    event.id.clone(),
                    reply_content.clone(),
                    1,
                )
                .await
            {
                Ok(outcome) => match outcome.created() {
                    Some(message) => {
                        info!(
                            message_id = %message.id,
                            channel_id = %event.channel_id,
                            "回复频道消息成功"
                        );
                    }
                    None => {
                        info!(channel_id = %event.channel_id, "回复未即时送达: {outcome:?}");
                    }
                },
                Err(e) => {
                    error!(error = %e, channel_id = %event.channel_id, "回复频道消息失败");
                }
            }
        })
    }
}
