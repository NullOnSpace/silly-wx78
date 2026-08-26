//! 只读冒烟测试：验证 guild 模块的日程(null 容忍)、精华、在线人数路径。
//! 用法: cargo run --example smoke_readonly
use qq_bot::{AuthClient, AuthConfig, GuildClient};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv()?;
    let http_client = Arc::new(reqwest::Client::new());
    let auth_client = Arc::new(AuthClient::new(
        AuthConfig::from_env()?,
        Arc::clone(&http_client),
    ));
    let client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));

    // 日程列表（空时平台返回字面量 null，库应容忍）
    match client.get_schedules("710455390", None).await {
        Ok(list) => println!("日程数量: {} (null 已容忍)", list.len()),
        Err(e) => println!("日程查询失败: {e}"),
    }

    // 精华消息
    match client.get_pins("9478690").await {
        Ok(pins) => println!("精华消息: {} 条", pins.len()),
        Err(e) => println!("精华查询失败: {e}"),
    }

    // 在线人数（语音子频道 9478693）
    match client.get_online_nums("9478693").await {
        Ok(n) => println!("语音子频道在线: {n}"),
        Err(e) => println!("在线人数失败(预期内): {e}"),
    }

    // 文字子频道调 online_nums → 预期 505007 错误（错误应被正确透传）
    match client.get_online_nums("9478690").await {
        Ok(n) => println!("文字子频道在线(意外成功): {n}"),
        Err(e) => println!("文字子频道在线人数按预期失败: {e}"),
    }

    // 子频道用户权限（机器人自己）
    match client
        .get_channel_user_permissions("9478690", "13370821155780965225")
        .await
    {
        Ok(p) => println!(
            "机器人权限位 {} → 查看:{} 管理:{} 发言:{}",
            p.permissions,
            p.can_view(),
            p.can_manage(),
            p.can_speak()
        ),
        Err(e) => println!("权限查询失败: {e}"),
    }

    Ok(())
}
