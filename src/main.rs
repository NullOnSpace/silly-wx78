use clap::{Parser, Subcommand};
use qq_bot::guild::{GuildClient, PatchChannelRequest};
use qq_bot::{
    AuthClient, AuthConfig, CancellationToken, ChannelMessageRequest, ChannelMessageSendOutcome,
    DefaultMessageHandler, DirectMessageClient, ShareLinkClient, WebSocketClient, WebSocketConfig,
    intents,
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
    /// 获取并显示access_token
    Auth,

    /// 发送单聊消息
    SendMessage {
        #[arg(short, long, help = "用户的openid")]
        openid: String,
        #[arg(short, long, help = "消息内容")]
        content: String,
    },

    /// 生成机器人分享链接
    GenerateLink {
        #[arg(short, long, help = "回调数据，用于追踪来源")]
        callback_data: Option<String>,
    },

    /// 启动WebSocket服务接收事件
    Websocket {
        #[arg(
            short,
            long,
            help = "事件订阅intents，可用 qq_bot::intents 常量组合；默认全量订阅",
            default_value_t = intents::ALL
        )]
        intents: i32,
    },

    /// 查询机器人自身信息
    Me,

    /// 列出机器人所在的频道(服务器)
    Guilds {
        #[arg(short, long, default_value_t = 100, help = "数量上限")]
        limit: u16,
    },

    /// 列出频道的子频道列表
    Channels { guild_id: String },

    /// 查看频道成员列表（自动翻页）
    Members {
        guild_id: String,
        #[arg(short, long, default_value_t = 50, help = "单页数量(<=50)")]
        limit: u16,
    },

    /// 查看身份组列表
    Roles { guild_id: String },

    /// 发送频道消息（带 --reply-to 为被动回复，否则为主动消息，可能进入安全审核）
    SendChannelMessage {
        #[arg(short, long, help = "子频道ID")]
        channel_id: String,
        #[arg(short, long, help = "消息内容")]
        content: String,
        #[arg(long, help = "被回复的消息ID（被动回复模式）")]
        reply_to: Option<String>,
        #[arg(long, default_value_t = 1, help = "msg_seq，同一msg_id第n次回复")]
        msg_seq: i32,
    },

    /// 撤回频道消息（2分钟窗口内；管理员可撤回成员消息）
    Recall {
        #[arg(short, long, help = "子频道ID")]
        channel_id: String,
        #[arg(short, long, help = "消息ID")]
        message_id: String,
    },

    /// 管理精华消息
    Pins {
        #[arg(short, long, help = "子频道ID")]
        channel_id: String,
        #[command(subcommand)]
        action: PinAction,
    },

    /// 查询机器人在频道的API权限
    ApiPermission { guild_id: String },

    /// 修改子频道名称（实测闭环通过）
    RenameChannel {
        #[arg(short, long, help = "子频道ID")]
        channel_id: String,
        #[arg(short, long, help = "新名称")]
        name: String,
    },
}

#[derive(Subcommand)]
enum PinAction {
    /// 查看精华消息列表（每子频道上限20条）
    List,
    /// 添加精华消息
    Add { message_id: String },
    /// 移除精华消息
    Remove { message_id: String },
}

fn print_send_outcome(outcome: &ChannelMessageSendOutcome) {
    match outcome {
        ChannelMessageSendOutcome::Created(message) => {
            println!("消息已创建:");
            println!("  ID: {}", message.id);
            if let Some(channel_id) = &message.channel_id {
                println!("  子频道: {channel_id}");
            }
        }
        ChannelMessageSendOutcome::Auditing(audit) => {
            println!("⚠️ 消息进入安全审核，未即时送达:");
            if let Some(audit_id) = &audit.audit_id {
                println!("  audit_id: {audit_id}");
            }
            println!("  code: {:?}", audit.code);
            println!("  提示: 需订阅 MESSAGE_AUDIT(1<<27) intents 才能收到审核结果");
        }
        ChannelMessageSendOutcome::Other(value) => {
            println!("未识别的响应: {value}");
        }
    }
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
            let message_client =
                DirectMessageClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));

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
            let share_link_client =
                ShareLinkClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));

            let url = if let Some(data) = callback_data {
                share_link_client
                    .generate_share_link_with_callback(data)
                    .await?
            } else {
                share_link_client.generate_share_link().await?
            };

            info!("生成分享链接成功");
            println!("分享链接: {}", url);
        }

        Commands::Websocket { intents } => {
            let dm_client =
                DirectMessageClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            let handler =
                Arc::new(DefaultMessageHandler::new(dm_client).with_guild_client(guild_client));
            let ws_config = WebSocketConfig::new(intents);
            let ws_client = Arc::new(WebSocketClient::new(
                Arc::clone(&auth_client),
                ws_config,
                Arc::clone(&http_client),
            ));

            info!(intents = intents, "启动WebSocket服务");
            let cancellation_token = CancellationToken::new();
            ws_client.run(handler, cancellation_token).await?;
        }

        Commands::Me => {
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            let me = guild_client.get_me().await?;
            println!("机器人ID: {}", me.id);
            println!("用户名: {}", me.username.as_deref().unwrap_or("(未知)"));
            if let Some(avatar) = &me.avatar {
                println!("头像: {avatar}");
            }
            if let Some(share_url) = &me.share_url {
                println!("分享链接: {share_url}");
            }
        }

        Commands::Guilds { limit } => {
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            for guild in guild_client.get_guilds(Some(limit)).await? {
                println!(
                    "[{}] {} (成员 {}/{})",
                    guild.id,
                    guild.name.as_deref().unwrap_or("(未命名)"),
                    guild.member_count.unwrap_or_default(),
                    guild.max_members.unwrap_or_default()
                );
            }
        }

        Commands::Channels { guild_id } => {
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            for channel in guild_client.get_channels(&guild_id).await? {
                println!(
                    "[{}] {:?} {} (parent={:?}, position={:?}){}",
                    channel.id,
                    channel.kind(),
                    channel.name.as_deref().unwrap_or("(未命名)"),
                    channel.parent_id,
                    channel.position,
                    if channel.is_category() {
                        " ← 分组，非真实子频道"
                    } else {
                        ""
                    }
                );
            }
        }

        Commands::Members { guild_id, limit } => {
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            for member in guild_client
                .get_members_all(&guild_id, limit.min(50), 20)
                .await?
            {
                println!(
                    "[{}] {} (身份组 {:?})",
                    member.user.as_ref().map(|u| u.id.as_str()).unwrap_or("?"),
                    member.display_name(),
                    member.roles
                );
            }
        }

        Commands::Roles { guild_id } => {
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            let roles = guild_client.get_roles(&guild_id).await?;
            for role in &roles.roles {
                println!(
                    "[{}] {} (人数 {:?}/{:?})",
                    role.id,
                    role.name.as_deref().unwrap_or("(未命名)"),
                    role.number,
                    role.member_limit
                );
            }
            if let Some(limit) = roles.role_num_limit {
                println!("身份组数量上限: {limit}");
            }
        }

        Commands::SendChannelMessage {
            channel_id,
            content,
            reply_to,
            msg_seq,
        } => {
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            let request = match &reply_to {
                // 被动回复：即时送达（msg_id 有效期5分钟、最多回复5次）
                Some(msg_id) => ChannelMessageRequest::reply(msg_id.clone(), content, msg_seq),
                // 主动消息：可能进入安全审核并消耗月度配额
                None => ChannelMessageRequest::text(content),
            };
            let outcome = guild_client
                .send_channel_message(&channel_id, request)
                .await?;
            print_send_outcome(&outcome);
        }

        Commands::Recall {
            channel_id,
            message_id,
        } => {
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            guild_client
                .recall_message(&channel_id, &message_id, true)
                .await?;
            println!("撤回成功");
        }

        Commands::Pins { channel_id, action } => {
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            match action {
                PinAction::List => {
                    for message in guild_client.get_pins(&channel_id).await? {
                        println!(
                            "[{}] {}",
                            message.id,
                            message.content.as_deref().unwrap_or("").trim()
                        );
                    }
                }
                PinAction::Add { message_id } => {
                    guild_client.add_pin(&channel_id, &message_id).await?;
                    println!("添加精华成功");
                }
                PinAction::Remove { message_id } => {
                    guild_client.remove_pin(&channel_id, &message_id).await?;
                    println!("移除精华成功");
                }
            }
        }

        Commands::ApiPermission { guild_id } => {
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            let list = guild_client.get_api_permission(&guild_id).await?;
            for api in &list.apis {
                println!(
                    "{:?} {} → 授权状态 {:?} ({})",
                    api.method,
                    api.path.as_deref().unwrap_or(""),
                    api.auth_status,
                    api.desc.as_deref().unwrap_or("")
                );
            }
        }

        Commands::RenameChannel { channel_id, name } => {
            let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
            let updated = guild_client
                .patch_channel(&channel_id, PatchChannelRequest::new().rename(name))
                .await?;
            println!(
                "修改成功，当前名称: {}",
                updated.name.as_deref().unwrap_or("(未知)")
            );
        }
    }

    Ok(())
}
