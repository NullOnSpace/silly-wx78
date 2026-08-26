# QQ Bot

基于 Rust 实现的 QQ 机器人 SDK，支持授权认证、单聊/频道消息收发、频道管理、分享链接生成和 WebSocket 事件监听。

## 功能

- **授权认证** — 通过 AppID 和 ClientSecret 获取 access_token，支持自动缓存和刷新
- **单聊消息** — 发送文本消息、回复消息、Markdown 消息（msg_type=2，原生/模板二选一）
- **频道(guild)接口** — 频道/子频道/成员/身份组查询、子频道改名、发消息与被动回复、撤回、精华消息、表情表态、日程管理、权限查询（详见 [docs/GUILD_API.md](docs/GUILD_API.md)）
- **分享链接** — 生成机器人邀请分享链接
- **WebSocket 服务** — 连接 QQ WebSocket 网关，维持心跳，自动重连与会话恢复；按事件类型分发：
  - `MESSAGE_CREATE` / `AT_MESSAGE_CREATE`（私域全量频道消息 / 公域@消息）
  - `GROUP_AT_MESSAGE_CREATE`（群@消息）
  - `DIRECT_MESSAGE_CREATE`（频道私信）
  - `GUILD_CREATE/UPDATE/DELETE`、`CHANNEL_CREATE/UPDATE/DELETE`
  - 未识别的事件统一进入 `on_unknown_event` 兜底回调，不再被静默丢弃

> 接口行为均经过真实请求验证，实测结论与坑点记录在 [docs/API_VERIFICATION.md](docs/API_VERIFICATION.md)。

## 配置

复制 `.env.example` 为 `.env` 并填入你的凭据：

```bash
cp .env.example .env
```

```env
QQ_BOT_APP_ID=your_app_id_here
QQ_BOT_CLIENT_SECRET=your_client_secret_here
```

## 构建

```bash
cargo build
```

## 使用

### 获取 access_token

```bash
cargo run -- auth
```

### 发送单聊消息

```bash
cargo run -- send-message -o <openid> -c <content>
```

### 频道相关命令

```bash
# 查询机器人信息 / 所在频道列表
cargo run -- me
cargo run -- guilds

# 列出子频道（会标注分组 category，分组不是真实子频道）
cargo run -- channels <guild_id>

# 成员与身份组
cargo run -- members <guild_id>
cargo run -- roles <guild_id>

# 发送频道消息：被动回复（即时送达）需带 --reply-to <msg_id>
cargo run -- send-channel-message -c <channel_id> -t "你好" --reply-to <msg_id>
# 主动消息（无 msg_id）：HTTP 200 但可能进入安全审核，返回 audit_id
cargo run -- send-channel-message -c <channel_id> -t "公告"

# 单聊 markdown 消息（被动回复带 --reply-to；不传 -m 发演示文案）
cargo run -- send-markdown -o <openid> -m "# 标题\n**加粗**" --reply-to <msg_id>
cargo run -- send-markdown -o <openid> --template <模板id> --param title=标题

# 撤回（2 分钟窗口内）/ 精华消息
cargo run -- recall -c <channel_id> -m <message_id>
cargo run -- pins -c <channel_id> list|add|remove

# 子频道改名
cargo run -- rename-channel -c <channel_id> -n "新名字"
```

### 启动 WebSocket 服务

```bash
cargo run -- websocket
# 自定义 intents（常用组合见 qq_bot::intents 常量）
cargo run -- websocket -i 100668929
```

默认 intents 为 `2113934851`（全量）。WebSocket 服务启动后：

- 收到单聊文本消息 → 回复 "Msg Got: <内容>"（历史行为不变）
- 收到频道消息 → 若配置了 GuildClient 则在原子频道被动回复 "Msg Got: <内容>"
- 当前为默认回复，可自定义回复逻辑（见下文）

### 平台频控与时限提醒

- 被动回复 msg_id 有效期约 **5 分钟**，同一 msg_id 最多回复 **5 次**，用 `msg_seq` 区分多次回复；
  markdown 同样适用（详见 [docs/DIRECT_MESSAGE.md](docs/DIRECT_MESSAGE.md)）；
- 主动消息需安全审核通道并消耗月度配额，不建议作为主流程依赖；
- 撤回窗口 **2 分钟**（自己的消息）；精华每子频道上限 **20 条**。

## 项目结构

```
src/
  auth.rs           — 授权认证模块
  direct_message.rs — 单聊消息模块
  guild.rs          — 频道(guild) REST 接口模块（消息/成员/身份组/精华/表态/日程等）
  handler.rs        — 默认事件处理器（单聊 + 频道消息回显）
  share_link.rs     — 分享链接模块
  websocket.rs      — WebSocket 连接、心跳、按类型分发的多场景事件模块
  main.rs           — 命令行入口
  lib.rs            — 库导出
docs/
  API_VERIFICATION.md — 开放平台接口实测验证报告（行为依据）
  GUILD_API.md        — 频道接口使用说明
```

## 自定义事件处理器

实现 `WebSocketHandler` trait 即可自定义事件处理逻辑。所有回调都有默认空实现，
按需覆写即可；未识别事件会进入 `on_unknown_event`：

```rust
use qq_bot::{AuthClient, AuthConfig, CancellationToken, DirectMessageClient, GuildClient,
             WebSocketClient, WebSocketConfig, WebSocketHandler, MessageEvent, ChannelMessageEvent};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

struct MyHandler {
    dm_client: Arc<DirectMessageClient>,
    guild_client: Arc<GuildClient>,
}

impl WebSocketHandler for MyHandler {
    // 单聊(C2C)文本消息
    fn on_message(&self, message: MessageEvent, _client: Arc<WebSocketClient>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let openid = message.author.user_openid.as_deref().unwrap_or(&message.author.id);
            let _ = self.dm_client.reply_message(openid, message.id, "自定义回复".to_string(), None).await;
        })
    }

    // 频道消息（私域 MESSAGE_CREATE 无需@机器人；公域 AT_MESSAGE_CREATE 需要@）
    fn on_channel_message(&self, event: ChannelMessageEvent, _client: Arc<WebSocketClient>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // 被动回复：msg_seq 从 1 开始递增（同一 msg_id 最多 5 次，5 分钟内有效）
            let _ = self.guild_client
                .reply_text(&event.channel_id, &event.id, format!("收到: {}", event.content), 1)
                .await;
        })
    }
}

// 启动时组装：
let http_client = Arc::new(reqwest::Client::new());
let config = AuthConfig::from_env()?;
let auth_client = Arc::new(AuthClient::new(config, Arc::clone(&http_client)));
let dm_client = DirectMessageClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
let guild_client = GuildClient::new(Arc::clone(&http_client), Arc::clone(&auth_client));
let handler = Arc::new(MyHandler { dm_client: Arc::new(dm_client), guild_client });

let ws_config = WebSocketConfig::new(qq_bot::intents::COMMON);
let ws_client = Arc::new(WebSocketClient::new(Arc::clone(&auth_client), ws_config, Arc::clone(&http_client)));
ws_client.run(handler, CancellationToken::new()).await?;
```

其他可覆写的回调：`on_group_message`（群@）、`on_direct_message`（频道私信）、
`on_guild_event` / `on_channel_event`（变更推送，注意 GUILD_UPDATE 触发频率不低，做缓存需去抖）。

## 日志

通过环境变量 `RUST_LOG` 控制日志级别：

```bash
RUST_LOG=qq_bot=debug cargo run -- websocket
```

## 许可证

MIT
