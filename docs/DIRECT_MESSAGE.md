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