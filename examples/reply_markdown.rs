//! 单聊 Markdown 被动回复示例。
//!
//! 运行后监听单聊(C2C)文本消息，用原生 markdown 回显用户内容：
//! ```bash
//! RUST_LOG=qq_bot=info cargo run --example reply_markdown
//! ```
//!
//! 要点（官方《Markdown 消息》文档）：
//! - 单聊发送侧 msg_type=2，markdown 内容放在请求体独立的 `markdown` 字段；
//! - 2026/04 起自定义 markdown 已开放所有机器人，无需申请模板；
//! - 被动回复的 msg_id 有效期约 5 分钟、同一 msg_id 最多回复 5 次，msg_seq 从 1 开始递增。
use qq_bot::{
    AuthClient, AuthConfig, CancellationToken, DirectMessageClient, MessageEvent, WebSocketClient,
    WebSocketConfig, WebSocketHandler, intents,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{error, info};

struct EchoHandler {
    dm_client: Arc<DirectMessageClient>,
}

impl WebSocketHandler for EchoHandler {
    fn on_message(
        &self,
        message: MessageEvent,
        _client: Arc<WebSocketClient>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let dm_client = Arc::clone(&self.dm_client);
        Box::pin(async move {
            info!(content = %message.content, "收到单聊消息，将以 markdown 回显");

            let openid = message
                .author
                .user_openid
                .as_deref()
                .unwrap_or(&message.author.id)
                .to_string();

            let markdown = format!(
                "# 收到你的消息\n---\n原文：{}\n\n> 这是 **markdown** 被动回复演示",
                message.content.replace('\n', "\n>")
            );

            // msg_seq=1：对该条消息的第 1 次回复；同一 msg_id 最多回复 5 次
            match dm_client
                .reply_markdown(&openid, &message.id, markdown, 1)
                .await
            {
                Ok(resp) => info!(message_id = %resp.id, "markdown 回复成功"),
                Err(e) => error!(error = %e, "markdown 回复失败"),
            }
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "qq_bot=info".into()),
        )
        .init();
    dotenvy::dotenv()?;

    let http_client = Arc::new(reqwest::Client::new());
    let auth_client = Arc::new(AuthClient::new(
        AuthConfig::from_env()?,
        Arc::clone(&http_client),
    ));
    let dm_client = Arc::new(DirectMessageClient::new(
        Arc::clone(&http_client),
        Arc::clone(&auth_client),
    ));

    let handler = Arc::new(EchoHandler { dm_client });
    // 单聊事件在 GROUP_AND_C2C_EVENT(1<<25) 中推送
    let ws_client = Arc::new(WebSocketClient::new(
        Arc::clone(&auth_client),
        WebSocketConfig::new(intents::GROUP_AND_C2C_EVENT | intents::GUILDS),
        Arc::clone(&http_client),
    ));

    ws_client.run(handler, CancellationToken::new()).await?;
    Ok(())
}
