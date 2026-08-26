# 单聊

## 发送
    说明
    单独发送消息给用户。

- 请求

**调用方式**
HTTP URL	`/v2/users/{openid}/messages`
HTTP Method	POST

- 路径参数
属性	类型	必填	说明
openid	string	是	QQ 用户的 openid，可在各类事件中获得。

- 请求参数
属性	类型	必填	说明
content	string	否	文本消息内容
msg_type	int	是	消息类型：0 文本、2 markdown、3 ark、4 embed、7 media 富媒体
markdown	object	否	Markdown 对象
keyboard	object	否	Keyboard 对象
ark	object	否	Ark 对象
media	object	否	富媒体单聊 的 file_info
message_reference	object	否	消息引用
event_id	string	否	前置收到的事件 ID，用于发送被动消息，支持事件："INTERACTION_CREATE"、"C2C_MSG_RECEIVE"、"FRIEND_ADD"
msg_id	string	否	前置收到的用户发送过来的消息 ID，用于发送被动（回复）消息
msg_seq	int	否	回复消息的序号，与 msg_id 联合使用，避免相同消息 id 回复重复发送，不填默认是 1。相同的 msg_id + msg_seq 重复发送会失败。
is_wakeup	bool	否	指明发送消息为互动召回消息，与 msg_id，event_id 互斥使用

- 返回参数
属性	类型	说明
id	string	消息唯一ID
timestamp	int	发送时间

- 常见错误码
code	message	说明
22009	msg limit exceed	消息发送超频
304082	upload media info fail	富媒体资源拉取失败，请重试
304083	convert media info fail	富媒体资源拉取失败，请重试

## 发送 Markdown 消息（msg_type=2）

> 依据官方文档《Markdown 消息》（2026/04/23 能力更新）：单聊、群聊场景的自定义
> Markdown 已开放**所有机器人**使用，无需申请模板；频道场景需内邀。

### 请求体协议

`markdown` 是请求体中独立的 object 字段（与 content 平级），字段如下：

| 属性 | 类型 | 必填 | 说明 |
|------|------|------|------|
| content | string | 否 | 自定义 markdown 文本内容 |
| custom_template_id | string | 否 | markdown 模版id，申请模版后获得 |
| params | Array | 否 | `{key: xxx, values: xxx}`，模版内变量与填充值的kv映射 |

`content` 与 `custom_template_id` **二选一**（库内构造与发送时均会校验）。
原生 markdown 形如：

```json
{
  "msg_type": 2,
  "msg_id": "<被动回复时携带>",
  "msg_seq": 1,
  "markdown": { "content": "# 标题 \n## 简介\n[🔗腾讯](https://www.qq.com)" }
}
```

### 库 API

```rust
// 原生自定义 markdown（主动发送，受审核/配额限制）
dm_client.send_markdown(&openid, "# 标题\n**加粗**").await?;

// 被动回复（推荐）：msg_id 来自收到的 C2C_MESSAGE_CREATE 事件，
// 有效期约 5 分钟，同一 msg_id 最多回复 5 次，msg_seq 从 1 开始递增
dm_client.reply_markdown(&openid, &event.id, "# 收到", 1).await?;

// 模板 markdown（模板 id 在开放平台管理端申请）
dm_client.send_markdown_template(&openid, "101993071_1658748972", vec![
    MarkdownParam::single("title", "标题"),
    MarkdownParam::new("link", vec!["https://www.qq.com".into()]),
]).await?;

// 或在任意 DirectMessageRequest 上链式附加（自动置 msg_type=Markdown 并清空互斥的 content）
let req = DirectMessageRequest::reply(msg_id, String::new(), None)
    .with_markdown(MarkdownPayload::native("# md"));
```

CLI：

```bash
# 原生 markdown（不传 -m 时发送演示文案）
cargo run -- send-markdown -o <openid> -m "# 标题\n**加粗**"
# 被动回复
cargo run -- send-markdown -o <openid> -m "# 回复" --reply-to <msg_id>
# 模板 + 变量
cargo run -- send-markdown -o <openid> --template <模板id> --param title=标题 --param link=https://qq.com
```

### 支持语法（节选）

`#`~`###` 标题；`**加粗**`、`__下划线加粗__`、`*斜体*`、`***粗斜体***`、`~~删除线~~`；
`[文字](url)` 链接与 `<https://doc.qq.com>` 裸链接；
`![说明 #宽px #高px](url)` 图片（公网可访问 url，平台会下载转存）；
有序/无序列表及嵌套（二级列表前空 4 格）；`>` 块引用；`***` 分割线。

### 实测渲染支持度（2026-08-26 真机核对）

| 语法 | 实测结果 |
|------|---------|
| `#`~`###` 标题 | ✅ 正常标题 |
| `####` 四级标题 | ✅ 存在，但与三级格式相同（等效三级） |
| `**加粗**` / `__下划线加粗__` / `*斜体*` / `***粗斜体***` / `~~删除线~~` | ✅ 全部正常 |
| 有序/无序列表 + 二级嵌套 | ✅ 缩进 4 空格生效 |
| `>` 块引用 / `>>> 嵌套引用` | ✅ 正常；嵌套显示为「引用线 + 缩进 + 引用线」 |
| `***` 分割线 | ✅ 正常 |
| `[文字](url)` | ✅ 可点击链接 |
| `<https://doc.qq.com>` 尖括号链接 | ✅ 可点击链接 |
| 纯文本 URL（如 `https://www.qq.com`） | ✅ 也会被自动识别为可点击链接 |
| `![说明 #宽px #高px](url)` 图片 | ✅ 正常，尺寸语法无明显变形 |
| **管道表格** `| A | B |` | ✅ 实测支持（官方文档未列，可用） |
| **代码块** ```` ``` ```` | ✅ 实测支持（官方文档未列，可用） |
| **行内代码** `` `code` `` | ✅ 仅变为等宽字体，无底色 |
| **任务列表** `- [ ]` / `- [x]` | ❌ 不识别，等价普通无序列表，`[ ]` 不显示 |
| **HTML 标签** `<b>` / `<font color>` | ❌ 被转义为纯文本显示 |
| 特殊字符转义 `\*` / `\_` / `\#` | ⚠️ 整体按普通文本显示；**裸单星号 `*文本*` 会解析成斜体** |
| emoji / 中文混排长文 | ✅ 排版正常，无乱码 |

> 结论：官方文档未列出但实测可用的有 **表格** 和 **代码块**；HTML 与任务列表不可用；
> 纯文本 URL 会自动转链接。

### 实测记录

- 用无效 openid 探测：服务端返回 `11255 用户不存在`（资源层错误）而非参数错误，
  说明 `msg_type=2 + markdown` 请求体格式被平台正常接受；
- 主动消息**仅凭 openid 即可发送成功**（无需 msg_id），实测返回正常消息 ID；
- 用假模板 id 探测模板消息：返回 `40034124 markdown消息参数错误`（模板通道需真实模板 id）；
- 单聊场景**只能发不能收** markdown（用户发来的 markdown 不产生事件内容）。

## 接收事件

### 单聊消息
    说明
    用户在单聊发送消息给机器人

- 基本概况
**基本**
intents	1<<25
事件类型	C2C_MESSAGE_CREATE
触发场景	用户在单聊发送消息给机器人
权限要求	无特殊要求
推送方式	Websocket

- 事件字段
属性	类型	说明
id	string	平台方消息ID，可以用于被动消息发送
author	object	发送者
content	string	文本消息内容
timestamp	string	消息生产时间（RFC3339）
attachments	object[]	富媒体文件附件，文件类型："图片，语音，视频，文件"

- author对象

属性	类型	说明
user_openid	string	用户 openid

- attachment对象

属性	类型	说明
content_type	string	文件类型，"image/jpeg","image/png","image/gif"，"file"，"video/mp4"，"voice"
filename	string	文件名称
height	int	图片高度
width	int	图片宽度
size	int	文件大小
url	string	文件链接
voice_wav_url	string	语音文件链接（wav格式）
asr_refer_text	string	语音 asr 参考结果

- 事件示例
```json
{
  "author": {
      "user_openid": "E4F4AEA33253A2797FB897C50B81D7ED"
  },
  "content": "123",
  "id": "ROBOT1.0_.b6nx.CVryAO0nR58RXuU6SC.m92gc19j02qKqdm8ek!",
  "timestamp": "2023-11-06T13:37:18+08:00"
}
```

- 其他说明
为了确保消息可到达，极端情况下，相同的 msg_id 的消息会有概率重复推送，当开发者在做“被动回复消息”响应业务的时候，如果开发者不对 msg_id 的回复做存储排重后的回复逻辑，很可能会回复了两条相同的消息给用户，这里我们引入了一个 msg_seq 的字段，便于过滤重复消息响应，可参考消息发送接口 msg_seq 的用法。