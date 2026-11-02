//! 频道模块只读接口探针：使用环境变量中的机器人凭据，逐个调用读接口并打印结果。
//!
//! 用法（凭据从当前目录或上级目录的 .env 加载）：
//! ```bash
//! cargo run --example api_probe
//! ```
//! 仅做只读调用，不发送/修改/删除任何数据。

use qq_bot::guild::{ChannelKind, GuildClient, ReactionType};
use qq_bot::{AuthClient, AuthConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let http_client = std::sync::Arc::new(reqwest::Client::new());
    let auth_client = std::sync::Arc::new(AuthClient::new(
        AuthConfig::from_env()?,
        std::sync::Arc::clone(&http_client),
    ));
    let client = GuildClient::new(std::sync::Arc::clone(&http_client), auth_client);

    let me = client.get_me().await?;
    println!(
        "== 机器人 {} ({})",
        me.username.as_deref().unwrap_or("?"),
        me.id
    );

    let guilds = client.get_guilds(None).await?;
    println!("== 所在频道数: {}", guilds.len());

    for guild in guilds.iter().take(3) {
        println!(
            "\n---- 频道 [{}] {} ----",
            guild.id,
            guild.name.as_deref().unwrap_or("?")
        );

        report(
            "get_guild",
            client.get_guild(&guild.id).await.map(|g| {
                format!(
                    "name={:?} members={:?}/{:?}",
                    g.name, g.member_count, g.max_members
                )
            }),
        );

        report(
            "get_members(limit=1)",
            client
                .get_members(&guild.id, Some(1), None)
                .await
                .map(|m| format!("{} 条", m.len())),
        );

        report(
            "get_roles",
            client
                .get_roles(&guild.id)
                .await
                .map(|r| format!("{} 个身份组, 上限 {:?}", r.roles.len(), r.role_num_limit)),
        );

        report(
            "get_role_members_page(role=1, limit=1)",
            client
                .get_role_members_page(&guild.id, "1", None, Some(1))
                .await
                .map(|p| format!("{} 人, next={:?}", p.data.len(), p.next)),
        );

        report(
            "get_api_permission",
            client
                .get_api_permission(&guild.id)
                .await
                .map(|p| format!("{} 条权限记录", p.apis.len())),
        );

        report(
            "get_message_setting",
            client.get_message_setting(&guild.id).await.map(|s| {
                format!(
                    "disable_create_dm={} disable_push={} channel_ids={:?} push_max={:?}",
                    s.disable_create_dm, s.disable_push_msg, s.channel_ids, s.channel_push_max_num
                )
            }),
        );

        let channels = match client.get_channels(&guild.id).await {
            Ok(c) => c,
            Err(e) => {
                println!("  [FAIL] get_channels: {e}");
                continue;
            }
        };
        println!("  get_channels: {} 个子频道", channels.len());

        for channel in channels.iter().take(12) {
            let name = channel.name.as_deref().unwrap_or("?");
            match channel.kind() {
                ChannelKind::Text => {
                    report(
                        &format!("get_pins({name})"),
                        client
                            .get_pins(&channel.id)
                            .await
                            .map(|p| format!("{} 条", p.len())),
                    );
                    report(
                        &format!("get_channel_user_permissions({name})"),
                        client
                            .get_channel_user_permissions(&channel.id, &me.id)
                            .await
                            .map(|p| {
                                format!(
                                    "permissions={} (view={} manage={} speak={})",
                                    p.permissions,
                                    p.can_view(),
                                    p.can_manage(),
                                    p.can_speak()
                                )
                            }),
                    );
                    if let Ok(pins) = client.get_pins(&channel.id).await
                        && let Some(message) = pins.first()
                    {
                        report(
                            &format!("get_reaction_users({name}, 赞)"),
                            client
                                .get_reaction_users(
                                    &channel.id,
                                    &message.id,
                                    ReactionType::SystemFace,
                                    "76",
                                    None,
                                    Some(10),
                                )
                                .await
                                .map(|p| format!("{} 人, is_end={}", p.users.len(), p.is_end())),
                        );
                    }
                }
                ChannelKind::Voice | ChannelKind::Live => {
                    report(
                        &format!("get_online_nums({name})"),
                        client
                            .get_online_nums(&channel.id)
                            .await
                            .map(|n| n.to_string()),
                    );
                }
                ChannelKind::Schedule => match client.get_schedules(&channel.id, None).await {
                    Ok(list) => {
                        println!("  [ OK ] get_schedules({name}): {} 条", list.len());
                        if let Some(first) = list.first().and_then(|s| s.id.as_deref()) {
                            report(
                                &format!("get_schedule({name})"),
                                client
                                    .get_schedule(&channel.id, first)
                                    .await
                                    .map(|s| format!("{:?}", s.map(|x| x.name))),
                            );
                        }
                    }
                    Err(e) => println!("  [FAIL] get_schedules({name}): {e}"),
                },
                ChannelKind::Post => {
                    report(
                        &format!("get_threads({name})"),
                        client.get_threads(&channel.id).await.map(|p| {
                            format!("{} 帖, is_finish={}", p.threads.len(), p.is_finish())
                        }),
                    );
                    if let Ok(page) = client.get_threads(&channel.id).await
                        && let Some(thread) = page
                            .threads
                            .first()
                            .and_then(|t| t.thread_info.as_ref())
                            .and_then(|i| i.thread_id.as_deref())
                    {
                        report(
                            &format!("get_thread({name})"),
                            client
                                .get_thread(&channel.id, thread)
                                .await
                                .map(|t| format!("{:?}", t.thread_info.map(|i| i.title))),
                        );
                    }
                }
                ChannelKind::Application => {
                    report(
                        &format!("get_pins(app {name})"),
                        client
                            .get_pins(&channel.id)
                            .await
                            .map(|p| format!("{} 条", p.len())),
                    );
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn report<T: std::fmt::Display>(label: &str, result: Result<T, impl std::fmt::Display>) {
    match result {
        Ok(v) => println!("  [ OK ] {label}: {v}"),
        Err(e) => println!("  [FAIL] {label}: {e}"),
    }
}
