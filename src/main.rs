use clap::{Parser, Subcommand};
use qq_bot::{
    AuthClient, AuthConfig, CancellationToken, DefaultMessageHandler, DirectMessageClient,
    ShareLinkClient, WebSocketClient, WebSocketConfig,
};
use reqwest::Client;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "qq-bot")]
#[command(about = "QQ机器人命令行工具", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "获取并显示access_token")]
    Auth,

    #[command(about = "发送单聊消息")]
    SendMessage {
        #[arg(short, long, help = "用户的openid")]
        openid: String,
        #[arg(short, long, help = "消息内容")]
        content: String,
    },

    #[command(about = "生成机器人分享链接")]
    GenerateLink {
        #[arg(short, long, help = "回调数据，用于追踪来源")]
        callback_data: Option<String>,
    },

    #[command(about = "启动WebSocket服务接收事件")]
    Websocket {
        #[arg(short, long, help = "事件订阅intents，例如: 1073741824 表示 PUBLIC_GUILD_MESSAGES", default_value = "2113934851")]
        intents: i32,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "qq_bot=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    dotenvy::dotenv()?;

    let cli = Cli::parse();

    let config = AuthConfig::from_env()?;
    let http_client = Arc::new(Client::new());
    let auth_client = Arc::new(AuthClient::new(config, Arc::clone(&http_client)));

    match cli.command {
        Commands::Auth => {
            let token = auth_client.get_access_token().await?;
            info!("成功获取access_token");
            println!("Access Token: {}", token);
        }

        Commands::SendMessage { openid, content } => {
            let message_client = DirectMessageClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));

            match message_client.send_text(&openid, content).await {
                Ok(response) => {
                    info!(
                        message_id = %response.id,
                        timestamp = response.timestamp,
                        "消息发送成功"
                    );
                    println!("消息ID: {}", response.id);
                    println!("时间戳: {}", response.timestamp);
                }
                Err(e) => {
                    error!(error = %e, "发送消息失败");
                    return Err(e.into());
                }
            }
        }

        Commands::GenerateLink { callback_data } => {
            let share_link_client = ShareLinkClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));

            let url = if let Some(data) = callback_data {
                share_link_client.generate_share_link_with_callback(data).await?
            } else {
                share_link_client.generate_share_link().await?
            };

            info!("生成分享链接成功");
            println!("分享链接: {}", url);
        }

        Commands::Websocket { intents } => {
            let handler = Arc::new(DefaultMessageHandler);
            let ws_config = WebSocketConfig::new(intents);
            let ws_client = Arc::new(WebSocketClient::new(Arc::clone(&auth_client), ws_config, Arc::clone(&http_client)));

            info!(intents = intents, "启动WebSocket服务");
            let cancellation_token = CancellationToken::new();
            ws_client.run(handler, cancellation_token).await?;
        }
    }

    Ok(())
}