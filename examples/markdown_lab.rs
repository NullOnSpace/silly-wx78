//! Markdown 实验室：监听单聊(C2C)消息，自动用不同形态的 markdown 被动回复，
//! 同时把 openid / msg_id 记录到 /tmp/qq_bot_inbox.jsonl 供后续 CLI 实验使用。
//!
//! 运行：`cargo run --example markdown_lab`
//! 用户每发一条消息 → 收到一条自动轮换样式的 markdown 回复（消耗该 msg_id 的 seq=1）；
//! 之后还可用 `cargo run -- send-markdown -o <openid> --reply-to <msg_id> --msg-seq N`
//! 在 5 分钟窗口内继续追加实验（同一 msg_id 上限 5 次回复）。
use qq_bot::{
    AuthClient, AuthConfig, CancellationToken, DirectMessageClient, MessageEvent, WebSocketClient,
    WebSocketConfig, WebSocketHandler, intents,
};
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{error, info};

const INBOX_PATH: &str = "/tmp/qq_bot_inbox.jsonl";

/// 官方文档示例图片（公网可访问，平台会下载转存）
const DEMO_IMAGE_URL: &str =
    "https://resource5-1255303497.cos.ap-guangzhou.myqcloud.com/abcmouse_word_watch/markdown/building.png";

struct LabHandler {
    dm_client: Arc<DirectMessageClient>,
    counter: AtomicUsize,
}

impl LabHandler {
    fn record_inbox(&self, openid: &str, msg_id: &str, content: &str) {
        let record = serde_json::json!({
            "openid": openid,
            "msg_id": msg_id,
            "content": content,
            "at": chrono_now_rfc3339(),
        });
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(INBOX_PATH)
        {
            let _ = writeln!(file, "{record}");
        }
    }
}

impl WebSocketHandler for LabHandler {
    fn on_message(
        &self,
        message: MessageEvent,
        _client: Arc<WebSocketClient>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let dm_client = Arc::clone(&self.dm_client);
        Box::pin(async move {
            let openid = message
                .author
                .user_openid
                .as_deref()
                .unwrap_or(&message.author.id)
                .to_string();

            println!("\n================ 收到单聊消息 ================");
            println!("openid : {openid}");
            println!("msg_id : {}", message.id);
            println!("content: {}", message.content);
            println!("=============================================\n");

            self.record_inbox(&openid, &message.id, &message.content);

            // 关键词触发或自动轮换
            let idx = self.counter.fetch_add(1, Ordering::SeqCst);
            let markdown = match message.content.trim() {
                "!img" | "！img" => sample_image(),
                "!list" => sample_list(),
                "!style" => sample_text_styles(),
                "!quote" => sample_quote(),
                _ => demo_by_index(idx),
            };
            let title = first_line(&markdown).to_string();

            match dm_client.reply_markdown(&openid, &message.id, markdown, 1).await {
                Ok(resp) => println!("✅ 已回复 markdown[{title}] message_id={}", resp.id),
                Err(e) => error!(error = %e, "markdown 回复失败"),
            }
        })
    }
}

fn first_line(md: &str) -> &str {
    md.lines().next().unwrap_or("")
}

fn demo_by_index(idx: usize) -> String {
    match idx % 4 {
        0 => sample_text_styles(),
        1 => sample_list(),
        2 => sample_quote(),
        _ => sample_image(),
    }
}

/// 文字样式全家桶（官方支持：加粗/下划线加粗/斜体/星号斜体/粗斜体/删除线/链接）
fn sample_text_styles() -> String {
    "# 文字样式演示

**加粗** __下划线加粗__ *斜体* ***粗斜体*** ~~删除线~~

行内链接：[🔗腾讯网](https://www.qq.com)
裸链接：<https://doc.qq.com>

正文结束".to_string()
}

/// 列表与嵌套
fn sample_list() -> String {
    "# 列表演示

- 无序一项
- 无序二项
    - 二级缩进项(前空4格)

1. 有序一项
2. 有序二项
    1. 二级有序项

列表前是普通文本需要空行隔开，否则无法识别".to_string()
}

/// 块引用 + 分割线
fn sample_quote() -> String {
    "# 引用与分割线

> 青青子衿，悠悠我心
> 但为君故，沉吟至今

***

分割线上方是 *** 三连字符

> 支持嵌套吗？
> > 再嵌一层".to_string()
}

/// 图片（官方示例资源 + 尺寸语法 #宽px #高px）
fn sample_image() -> String {
    format!(
        "# 图片演示

![建筑图 #208px #320px]({DEMO_IMAGE_URL})

图片需公网可访问，平台会下载转存"
    )
}

fn chrono_now_rfc3339() -> String {
    // 无 chrono 依赖，用系统秒级时间戳代替
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format!("{secs}")
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
    let auth_client = Arc::new(AuthClient::new(AuthConfig::from_env()?, Arc::clone(&http_client)));
    let dm_client = Arc::new(DirectMessageClient::new(
        Arc::clone(&http_client),
        Arc::clone(&auth_client),
    ));

    let handler = Arc::new(LabHandler {
        dm_client,
        counter: AtomicUsize::new(0),
    });

    let ws_client = Arc::new(WebSocketClient::new(
        Arc::clone(&auth_client),
        WebSocketConfig::new(intents::GROUP_AND_C2C_EVENT),
        Arc::clone(&http_client),
    ));

    info!("🧪 Markdown 实验室已启动，等待单聊消息…（收件箱: {INBOX_PATH}）");
    println!("🧪 Markdown 实验室已启动，给机器人发条单聊消息试试。");
    println!("   关键词: !style 文字样式 | !list 列表 | !quote 引用 | !img 图片");
    println!("   其余内容将自动轮换样式回复\n");

    let cancellation_token = CancellationToken::new();
    {
        let token = cancellation_token.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("收到 Ctrl-C，退出实验室");
            token.cancel();
        });
    }

    ws_client.run(handler, cancellation_token).await?;
    Ok(())
}
