# 频道(guild)接口使用说明

> 实现依据：[API_VERIFICATION.md](API_VERIFICATION.md) 的真实请求验证结论（2026-08-26）。
> 本文列出 `GuildClient` 提供的全部方法与实测注意事项。

## 快速开始

```rust
use qq_bot::{AuthClient, AuthConfig, GuildClient};
use std::sync::Arc;

let http_client = Arc::new(reqwest::Client::new());
let auth_client = Arc::new(AuthClient::new(AuthConfig::from_env()?, Arc::clone(&http_client)));
let guild = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));

// 机器人信息
let me = guild.get_me().await?;

// 列出所在频道与子频道
for g in guild.get_guilds(None).await? { /* ... */ }
for ch in guild.get_channels(&g.id).await? {
    // 注意：分组(category) 不是真实子频道，调消息类接口会报「频道不存在」
    if !ch.is_category() { /* ... */ }
}
```

## 方法清单

### 第一档（主流程）

| 方法 | 接口 | 说明 |
|------|------|------|
| `get_me` | GET /users/@me | 含 share_url |
| `get_guilds(limit)` | GET /users/@me/guilds | |
| `get_guild` / `get_channels` / `get_channel` | GET ... | 子频道类型见下表 |
| `patch_channel` | PATCH /channels/{cid} | 改名等，实测改名→恢复闭环通过 |
| `get_members` / `get_members_all` | GET /guilds/{gid}/members | index 翻页已封装 |
| `get_member` | GET /guilds/{gid}/members/{uid} | |
| `get_roles` / `create_role` / `modify_role` / `delete_role` | roles 系列 | 创建响应取 `role_id` 字段！`hoist` 返回整数 1/0（库已兼容） |
| `get_role_members_page` / `get_role_members_all` | roles/{rid}/members | next 游标翻页已封装 |
| `get_channel_user_permissions` / `get_channel_role_permissions` | permissions | 位图字符串：1查看/2管理/4发言 |
| `get_api_permission` | api_permission | 消息类接口实测均已授权 |
| `send_channel_message` / `send_text` / `reply_text` | POST messages | 返回值见下文"发送结果" |
| `get_message` | GET messages/{mid} | 响应有 `{message:...}` 包装，已在库内解包 |
| `recall_message` | DELETE messages/{mid} | 2 分钟窗口；204/空体 |

### 第二档

| 方法 | 接口 | 说明 |
|------|------|------|
| `get_pins` / `add_pin` / `remove_pin` | pins | 每子频道上限 20 条；空体容忍 |
| `add_reaction` / `delete_reaction` / `get_reaction_users` | reactions | 重复表态报 620003，建议幂等处理 |
| `get_schedules` / `get_schedule` / `create_schedule` / `delete_schedule` | schedules | 见下文"日程坑点" |
| `get_online_nums` | online_nums | 仅语音/直播子频道(type=1/2)有效 |
| `get_message_setting` | message/setting | 未确认字段落入 `extra` |

### 子频道类型（ChannelKind）

| type | 含义 | 备注 |
|------|------|------|
| 0 | 文字子频道 `Text` | 可收发消息 |
| 1 | 直播子频道 `Live` | 文档值，未在验证中出现 |
| 2 | 语音子频道 `Voice` | online_nums 可用 |
| 4 | 分组 `Category` | 非真实子频道，多数接口报错 |
| 10007 | 帖子/论坛 `Post` | 不产生文字消息事件 |
| 10011 | 日程 `Schedule` | schedules 接口目标 |

## 发送结果（主动消息审核）

实测：不带 msg_id 的主动消息可能 **HTTP 200 但进入安全审核**（body 含 `code=304023` + `audit_id`），
因此发送接口返回枚举而非直接返回消息：

```rust
match guild.reply_text(&channel_id, &msg_id, "回复", 1).await? {
    ChannelMessageSendOutcome::Created(msg) => println!("已送达: {}", msg.id),
    ChannelMessageSendOutcome::Auditing(audit) => println!("进入审核: {:?}", audit.audit_id),
    ChannelMessageSendOutcome::Other(value) => println!("未知响应: {value}"),
}
```

被动回复规则：msg_id 有效期约 5 分钟、同一 msg_id 最多回复 5 次、msg_seq 从 1 开始递增去重。
如需审核结果，Identify 时需包含 MESSAGE_AUDIT(1<<27) intents。

## 日程坑点

- 空列表时平台返回**字面量 `null`** —— 库已容忍并返回空 `Vec`；
- 时间戳是**毫秒字符串**（序列化/反序列化均已在库内处理）；
- 字段名必须是 `remind_type`（不是 reminder_type），请求自动包 `{"schedule": {...}}` 信封；
- 删除日程在验证期间持续返回平台故障 `50015001 系统繁忙`，请做好重试兜底。

## 已知不可用/未覆盖接口（勿依赖）

- 创建子频道：200010 机器人资质不足；
- 公告（子频道/全局）：501002 废弃 / 501006 条件不足，替代品=精华消息；
- 私信会话创建（POST /users/@me/dms）：305101，guild 未开通频道私信；
- 音频控制：仅音频类机器人可用；
- 成员禁言、踢人、成员身份组增删：影响他人状态，留待真实需求再做。

## 补充实测（2026-08-26 晚，本库联调期间）

- `GET /guilds/{gid}/roles` 的 `hoist` 字段是**整数 `1`/`0`** 而非布尔值，严格 bool 反序列化
  会失败；`Role::hoist` 已用宽松解析兼容布尔与 0/1；
- `GET /users/@me` 偶发平台故障 `50015009 系统繁忙`（HTTP 500），重试即可；
- `GET /channels/{cid}/online_nums` 对文字子频道实测返回 `505007 requested channel is not
  audio or live channel`，错误信息已原样透传。
