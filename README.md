# QQ Bot

基于 Rust 实现的 QQ 机器人 SDK，支持授权认证、单聊消息收发、分享链接生成和 WebSocket 事件监听。

## 功能

- **授权认证** — 通过 AppID 和 ClientSecret 获取 access_token，支持自动缓存和刷新
- **单聊消息** — 发送文本消息和回复消息
- **分享链接** — 生成机器人邀请分享链接
- **WebSocket 服务** — 连接 QQ WebSocket 网关，维持心跳，接收并处理事件

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

### 生成分享链接

```bash
cargo run -- generate-link
cargo run -- generate-link -d <callback_data>
```

### 启动 WebSocket 服务

```bash
cargo run -- websocket
cargo run -- websocket -i <intents>
```

默认 intents 为 `2113934851`。

WebSocket 服务启动后，收到文本消息会自动回复 "Msg Got: <消息内容>"。
当前为默认回复，可自定义回复逻辑。

## 项目结构

```
src/
  auth.rs           — 授权认证模块
  direct_message.rs — 单聊消息模块
  handler.rs        — 默认事件处理器
  share_link.rs     — 分享链接模块
  websocket.rs      — WebSocket 连接与事件模块
  main.rs           — 命令行入口
  lib.rs            — 库导出
```

## 自定义事件处理器

实现 `WebSocketHandler` trait 即可自定义事件处理逻辑：

```rust
use qq_bot::{CancellationToken, DirectMessageClient, WebSocketClient, WebSocketHandler, MessageEvent};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

struct MyHandler;

impl WebSocketHandler for MyHandler {
    fn on_message(&self, message: MessageEvent, client: Arc<WebSocketClient>) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let openid = message.author.user_openid.as_deref().unwrap_or(&message.author.id);
            let dm_client = DirectMessageClient::new(
                Arc::clone(client.http_client()),
                Arc::clone(client.token()),
            );
            dm_client.reply_message(
                openid,
                message.id,
                "自定义回复".to_string(),
                None,
            ).await.ok();
        })
    }
}
```

在启动时传入自定义处理器：

```rust
let http_client = Arc::new(reqwest::Client::new());
let config = AuthConfig::from_env()?;
let auth_client = AuthClient::new(config, Arc::clone(&http_client));
let token = auth_client.get_access_token().await?;

let ws_config = WebSocketConfig::new(token, intents);
let ws_client = Arc::new(WebSocketClient::new(ws_config, Arc::clone(&http_client)));
ws_client.run(handler, CancellationToken::new()).await?;
```

## 日志

通过环境变量 `RUST_LOG` 控制日志级别：

```bash
RUST_LOG=qq_bot=debug cargo run -- websocket
```

## 许可证

MIT