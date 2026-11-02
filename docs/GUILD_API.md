# 频道(guild)接口使用说明

> 实现依据：QQ 机器人官方文档 api-v2《频道模块》《消息收发/频道消息》，
> 行为细节以 [API_VERIFICATION.md](API_VERIFICATION.md) 的真实请求验证结论为准（2026-08-26）。
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

### 频道与子频道

| 方法 | 接口 | 说明 |
|------|------|------|
| `get_me` | GET /users/@me | 含 share_url |
| `get_guilds(limit)` | GET /users/@me/guilds | |
| `get_guild` / `get_channels` / `get_channel` | GET ... | 子频道类型见下表 |
| `create_channel` | POST /guilds/{gid}/channels | 私域接口；实测当前机器人资质不足报 200010 |
| `patch_channel` | PATCH /channels/{cid} | 改名等，实测改名→恢复闭环通过 |
| `delete_channel` | DELETE /channels/{cid} | 私域接口，删除后不可恢复 |

### 成员、身份组与权限

| 方法 | 接口 | 说明 |
|------|------|------|
| `get_members` / `get_members_all` | GET /guilds/{gid}/members | index 翻页已封装 |
| `get_member` | GET /guilds/{gid}/members/{uid} | |
| `delete_member` | DELETE /guilds/{gid}/members/{uid} | 可加黑名单/撤回历史消息；无法移除管理员 |
| `get_roles` / `create_role` / `modify_role` / `delete_role` | roles 系列 | 创建响应取 `role_id` 字段！`hoist` 返回整数 1/0（库已兼容） |
| `add_member_role` / `remove_member_role` | PUT/DELETE /guilds/{gid}/members/{uid}/roles/{rid} | 身份组 5（子频道管理员）需传 `channel_id` |
| `get_role_members_page` / `get_role_members_all` | roles/{rid}/members | next 游标翻页已封装 |
| `get_channel_user_permissions` / `get_channel_role_permissions` | permissions | 位图字符串：1查看/2管理/4发言 |
| `update_channel_user_permissions` / `update_channel_role_permissions` | PUT permissions | `ChannelPermissionModify::new().grant_view()...`；不支持改「可管理」 |
| `get_api_permission` | api_permission | 消息类接口实测均已授权 |
| `create_api_permission_demand` | POST api_permission/demand | 接口权限授权链接，每天每频道默认 3 条 |

### 发言管理

| 方法 | 接口 | 说明 |
|------|------|------|
| `get_message_setting` | GET /guilds/{gid}/message/setting | 字段：disable_create_dm/disable_push_msg/channel_ids/channel_push_max_num |
| `mute_guild` | PATCH /guilds/{gid}/mute | 全员禁言，`MuteRequest::unmute()` 解除 |
| `mute_member` | PATCH /guilds/{gid}/members/{uid}/mute | 指定成员禁言 |
| `mute_members` | PATCH /guilds/{gid}/mute | 批量禁言，返回成功设置的 user_ids |

### 频道消息

| 方法 | 接口 | 说明 |
|------|------|------|
| `send_channel_message` / `send_text` / `reply_text` | POST messages | 支持 content/image/embed/ark/markdown/message_reference，返回值见下文"发送结果" |
| `send_channel_message_multipart` | POST messages (form-data) | 本地图片 `file_image` 上传 |
| `get_message` | GET messages/{mid} | 响应有 `{message:...}` 包装，已在库内解包 |
| `recall_message` | DELETE messages/{mid} | 2 分钟窗口；204/空体 |
| `create_dms_session` | POST /users/@me/dms | 创建频道私信会话 |
| `send_dms_message` / `recall_dms_message` | POST/DELETE /dms/{gid}/messages | 参数与频道消息一致 |

### 内容管理（精华 / 日程 / 公告 / 音频 / 论坛）

| 方法 | 接口 | 说明 |
|------|------|------|
| `get_pins` / `add_pin` / `remove_pin` | pins | 每子频道上限 20 条；空体容忍 |
| `get_schedules` / `get_schedule` / `create_schedule` / `patch_schedule` / `delete_schedule` | schedules | 见下文"日程坑点" |
| `create_announce` / `delete_announce` | /guilds/{gid}/announces | 消息公告或推荐子频道公告（≤3 条，`delete` 传 `all` 清除） |
| `audio_control` / `put_mic` / `delete_mic` | /channels/{cid}/audio、/mic | 仅音频类机器人可用 |
| `get_threads` / `get_thread` / `create_thread` / `delete_thread` | /channels/{cid}/threads | 论坛子频道，仅私域可用 |

### 表情表态 / 在线人数

| 方法 | 接口 | 说明 |
|------|------|------|
| `add_reaction` / `delete_reaction` / `get_reaction_users` | reactions | 类型 `ReactionType::SystemFace`(1)/`Emoji`(2)；重复表态报 620003 |
| `get_online_nums` | online_nums | 仅语音(2)/直播(10005)子频道有效 |

### 子频道类型（ChannelKind）

| type | 含义 | 备注 |
|------|------|------|
| 0 | 文字子频道 `Text` | 可收发消息 |
| 2 | 语音子频道 `Voice` | online_nums 可用 |
| 4 | 分组 `Category` | 非真实子频道，多数接口报错 |
| 10005 | 直播子频道 `Live` | online_nums 可用 |
| 10006 | 应用子频道 `Application` | 如王者开黑大厅、日程提醒 |
| 10007 | 论坛子频道 `Post` | 不产生文字消息事件 |
| 10011 | 日程子频道 `Schedule` | 实测出现值，schedules 接口目标 |
| 1/3 | 保留值 `Unknown` | 官方标注不可用 |

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

## 已知不可用/条件不足的接口（已实现但勿强依赖）

- 创建子频道：接口已实现，但实测返回 `200010 机器人资质不足`；
- 子频道公告（POST /channels/{cid}/announces）：501002 已废弃，替代品=精华消息；
  全局公告（/guilds/{gid}/announces）已实现，实测 501006 条件不足；
- 私信会话创建（POST /users/@me/dms）：已实现，实测 `305101`（该 guild 未开通频道私信）；
- 音频控制 / 上麦 / 下麦：接口已实现，仅音频类机器人可用；
- 论坛帖子（threads）：接口已实现，仅私域机器人可用；
- 影响他人状态的接口（删除成员、禁言、身份组增删、权限修改）已按文档实现，
  调用前请确认机器人具备对应管理员权限，且遵守平台运营规范。

## WebSocket 事件补充

`WebSocketHandler` 新增以下频道相关回调（默认空实现）：

| 回调 | 事件 |
|------|------|
| `on_guild_member_event` | GUILD_MEMBER_ADD / UPDATE / REMOVE |
| `on_message_audit_pass` / `on_message_audit_reject` | MESSAGE_AUDIT_PASS / REJECT（主动消息审核结果） |
| `on_message_reaction` | MESSAGE_REACTION_ADD / REMOVE |
| `on_audio_or_live_channel_member` | AUDIO_OR_LIVE_CHANNEL_MEMBER_ENTER / EXIT |

## 补充实测（2026-08-26 晚，本库联调期间）

- `GET /guilds/{gid}/roles` 的 `hoist` 字段是**整数 `1`/`0`** 而非布尔值，严格 bool 反序列化
  会失败；`Role::hoist` 已用宽松解析兼容布尔与 0/1；
- `GET /users/@me` 偶发平台故障 `50015009 系统繁忙`（HTTP 500），重试即可；
- `GET /channels/{cid}/online_nums` 对文字子频道实测返回 `505007 requested channel is not
  audio or live channel`，错误信息已原样透传。

## 真实凭证实测补充（2026-10-02，「饥荒助手内测」guild）

> 只读全量探针：`cargo run --example api_probe`（凭据从 `.env` 读取，只读不写）。

- 主动消息发送受平台时段限制：00:00–05:59 推送返回 `304022 / 40034111`
  「主动消息不能在00:00:00 - 05:59:59 推送」，需白天时段调用；
- 子频道权限修改的平台语义（库按文档透传，失败时为 `301003`）：
  - 用户权限（PUT .../members/{uid}/permissions）要求子频道为**指定成员可见**
    （`private_type=2`）且目标不是频道主/管理员；公开频道调用会失败；
  - 身份组权限（PUT .../roles/{rid}/permissions）在 `private_type=2` 子频道下实测成功；
- 禁言：`mute_guild` 正数时长/解除均实测成功；指定成员/批量禁言对管理员返回 `502008`，
  对「频道助手」类系统账号也可能 `502008`（无普通成员可测）；
  批量禁言空 `user_ids` 时平台返回空 body，库已容忍为空结果；
- 论坛帖子 `create_thread` → 轮询 `get_threads` → `delete_thread` 实测闭环成功；
- 推荐子频道公告创建返回 `501020`（平台侧拒绝，替代品为消息公告/精华消息）；
- `create_channel` 返回 `200010`；`create_dms_session` 返回 `305101`（与 8 月结论一致）。
