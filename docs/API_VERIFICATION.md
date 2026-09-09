# QQ 机器人开放接口验证报告（api-v2）

> 验证时间：2026-08-26 19:00–19:40 CST
> 验证方式：Python 脚本直连 REST/WSS 实测（脚本位于验证机 `/tmp/qqverify/`，原始事件日志 `events_full.jsonl`）
> 目的：为 `qq-bot` 库的频道消息 / 频道管理功能扩展提供实测依据。所有结论均以真实请求为准，非文档转述。

## 一、验证环境

| 项目 | 值 |
|------|-----|
| 机器人 | 「饥荒联机信息助手」AppID `19035***589` |
| 机器人用户 ID | `13370821155780965225` |
| 测试频道(guild) | 「饥荒助手内测」 id=`17327925269880161597`，3 成员，机器人具有**管理员身份组**(role 2) |
| 测试文字子频道 | 「活动互助」 id=`9478690`（曾临时改名验证 PATCH 后恢复） |
| 机器人类型 | **私域机器人**（关键前提，见下文） |
| REST 域名 | `https://api.sgroup.qq.com` ✅（文档中另一域名 `api.bot.qq.com` 未采用） |

## 二、接入基础（实测确认）

### 2.1 访问凭证

```
POST https://bots.qq.com/app/getAppAccessToken
Content-Type: application/json
{"appId": "...", "clientSecret": "..."}
```

响应（注意 `expires_in` 是**字符串**）：

```json
{"access_token": "YBXs...(长token)", "expires_in": "2360"}
```

- 调用业务接口时带请求头：`Authorization: QQBot <access_token>`
- 有效期约 7200s，实测下发 2360s；过期前 60s 内重复获取会拿到新 token，旧 token 在这 60s 内仍有效

### 2.2 WSS 网关协议（Python 原型已完整跑通）

- 地址：`GET /gateway` → `{"url": "wss://api.sgroup.qq.com/websocket"}`（无需 `/gateway/bot` 即可正常收事件）
- 握手时序：
  1. 服务端推 `op=10 Hello`，`d.heartbeat_interval` 毫秒（实测 41250ms）
  2. 客户端发 `op=2 Identify`：`{"token":"QQBot <access_token>", "intents": <位图>, "properties":{"$os","$browser","$device"}}`
  3. 服务端推 dispatch `t=READY`：`{"version":1,"session_id":"<uuid>","user":{...},"shard":[0,0]}`
  4. 心跳：客户端每 interval 发 `{"op":1,"d":<last_seq>}`，服务端回 `op=11`（也可不等待）
- 连接时**不需要**在 HTTP 头里带 Authorization
- 实测 intents `100668929` 可正常连接并收到频道事件：
  `GUILDS(1<<0) | GUILD_MESSAGES(1<<9) | GUILD_MESSAGE_REACTIONS(1<<10) | DIRECT_MESSAGE(1<<12) | GROUP_AND_C2C_EVENT(1<<25) | INTERACTION(1<<26)`
- 项目现用值 `2113934851`（含全部 intents）同样能连接成功——即**订阅侧早已就绪，只是分发层丢弃了非 C2C 事件**

### 2.3 私域机器人语义（本次最重要的认知）

- 收到的频道消息事件是 **`MESSAGE_CREATE`（全量消息）**，而非公域的 `AT_MESSAGE_CREATE`
- 用户**不需要 @机器人**，任意文字子频道的普通消息都会推送
- 整个测试期间从未出现过 `AT_MESSAGE_CREATE`
- 若库要同时兼容公域，需同时处理两种事件类型（payload 结构基本一致）

## 三、接口验证总表

✅=实测通过 ❌=实测失败 ⚠️=通过但有重要附加行为 未测=未覆盖

| 分类 | 方法 & 路径 | 结果 | 备注 |
|------|------------|------|------|
| 凭证 | POST getAppAccessToken | ✅ | expires_in 为字符串 |
| 身份 | GET /users/@me | ✅ | 含 share_url 字段 |
| 网关 | GET /gateway | ✅ | |
| 频道列表 | GET /users/@me/guilds | ✅ | 数组元素含 member_count/max_members/joined_at |
| 频道详情 | GET /guilds/{guild_id} | ✅ | 结构同上 |
| 子频道列表 | GET /guilds/{guild_id}/channels | ✅ | 见 4.3 类型表 |
| 子频道详情 | GET /channels/{channel_id} | ✅ | |
| 改子频道名 | PATCH /channels/{channel_id} | ✅ | 返回更新后对象；已做改名→恢复闭环 |
| 创建子频道 | POST /guilds/{guild_id}/channels | ❌ 200010 | `create channel error`，疑机器人资质不足 |
| 在线人数 | GET /channels/{channel_id}/online_nums | ✅/❌ | 仅 type=2 语音/直播有效；文字子频道 505007；分组(type=4) 400「频道不存在」 |
| 成员列表 | GET /guilds/{guild_id}/members?limit=1 | ✅ | 返回 Member 数组 |
| 成员详情 | GET /guilds/{guild_id}/members/{user_id} | ✅ | |
| 身份组列表 | GET /guilds/{guild_id}/roles | ✅ | `{guild_id, roles[], role_num_limit:"100"}` |
| 创建身份组 | POST /guilds/{guild_id}/roles | ✅⚠️ | 响应是 `{role_id:"...", role:null}`，**取 `role_id` 而非 `id`** |
| 删除身份组 | DELETE /guilds/{guild_id}/roles/{role_id} | ✅ | 204 空体 |
| 修改身份组 | PATCH /guilds/{guild_id}/roles/{role_id} | 未测 | 因上条响应字段踩坑跳过 |
| 身份组成员 | GET /guilds/{guild_id}/roles/{role_id}/members | ✅ | `{data:[Member], next:"0"}` 游标分页 |
| 子频道用户权限 | GET /channels/{cid}/members/{uid}/permissions | ✅ | 机器人权限位 `"7"`（查看+管理+发言） |
| 子频道身份组权限 | GET /channels/{cid}/roles/{rid}/permissions | ✅ | |
| API 权限 | GET /guilds/{guild_id}/api_permission | ✅ | 消息类接口 auth_status 全部=1 |
| 消息频率设置 | GET /guilds/{guild_id}/message/setting | ✅ | |
| 发消息(被动) | POST /channels/{cid}/messages | ✅ | 带 msg_id+msg_seq；两次实测均即时送达 |
| 发消息(主动) | POST /channels/{cid}/messages | ⚠️ | HTTP 200 但进入安全审核：`code=304023` + `audit_id`，不即时送达 |
| 获取指定消息 | GET /channels/{cid}/messages/{mid} | ✅⚠️ | 响应包一层 `{message:{...}}`，且返回的 id 是**更长的内部 id**（原 id 是其尾部子串），操作仍应使用原 id |
| 撤回消息 | DELETE /channels/{cid}/messages/{mid} | ✅ | 撤回自己 2 分钟内的被动回复成功，返回 `200 {}` |
| 精华消息 | GET/PUT/DELETE /channels/{cid}/pins[/{mid}] | ✅ | 增查删闭环完成 |
| 表情表态 | PUT/GET/DELETE /channels/{cid}/messages/{mid}/reactions/{type}/{id} | ✅⚠️ | PUT 重复表态报 500 `620003 already had reaction`；PUT/DELETE 成功时 204 空体 |
| 日程列表 | GET /channels/{cid}/schedules | ✅ | 空列表返回字面量 `null`（反序列化要容忍！） |
| 创建日程 | POST /channels/{cid}/schedules | ✅⚠️ | 字段是 `remind_type`（不是 reminder_type）；时间戳是**毫秒**字符串 |
| 删除日程 | DELETE /channels/{cid}/schedules/{sid} | ❌ 平台故障 | 持续 `50015001 系统繁忙`（重试 7 次跨 8 分钟），测试日程需手动删除 |
| 子频道公告 | POST /channels/{cid}/announces | ❌ 501002 | 文档标注 2022-03 废弃，替代品=精华消息 |
| 全局公告 | POST /guilds/{gid}/announces | ❌ 501006 | 疑需精华前置或更高资质 |
| 私信会话 | POST /users/@me/dms | ❌ 305101 | 该 guild 未开通频道私信；api_permission 中也无任何 dms 条目 |
| 音频控制 | POST /channels/{cid}/audio | 未测 | 文档注明仅音频类机器人可用 |

## 四、实测数据结构摘录

### 4.1 MESSAGE_CREATE（私域全量频道消息，完整 payload）

```json
{
  "author": {
    "avatar": "https://qqchannel-profile-1251316161.file.myqcloud.com/...",
    "bot": false,
    "id": "5688363186875882260",
    "union_openid": "",
    "username": "besiege"
  },
  "channel_id": "9478690",
  "content": "wawa",
  "guild_id": "17327925269880161597",
  "id": "08bd8ae3ad9c87cabcf00110a2c4c204380348b79abbd406",
  "member": {
    "joined_at": "2022-08-01T09:06:33+08:00",
    "nick": "besiege",
    "roles": ["4", "5", "14"]
  },
  "seq": 3,
  "seq_in_channel": "3",
  "timestamp": "2026-08-26T19:25:43+08:00"
}
```

注意：频道事件的 author **没有** `user_openid` 字段（C2C 场景才有）；身份信息走 `member.roles/nick`。

### 4.2 CHANNEL_UPDATE / GUILD_UPDATE（变更后数据）

```json
// CHANNEL_UPDATE —— 注意多了 op_user_id（操作者）
{
  "application_id": "0",
  "guild_id": "17327925269880161597",
  "id": "9478690",
  "name": "活动互助",
  "op_user_id": "5688363186875882260",
  "owner_id": "5688363186875882260",
  "parent_id": "644066500",
  "permissions": "3",
  "position": 2,
  "private_type": 0,
  "speak_permission": 1,
  "sub_type": 0,
  "type": 0
}
```

GUILD_UPDATE payload 与 `GET /guilds/{id}` 结构一致（description/icon/id/joined_at/max_members/member_count/name/owner_id）。改名、进出频道等都会触发，**频率不低**，若做缓存要注意去抖。

### 4.3 子频道类型实测枚举（本 guild）

| type | 含义 | 备注 |
|------|------|------|
| 0 | 文字子频道 | 可收发消息 |
| 2 | 语音子频道 | online_nums 可用 |
| 4 | 分组(category) | 不是真实子频道，对它调接口报「频道不存在」 |
| 10007 | 帖子/论坛区 | 不产生文字消息事件 |
| 10011 | 日程子频道 | schedules 接口的目标 |

### 4.4 权限位图（permissions 字符串）

`1`=可查看，`2`=可管理，`4`=可发言（并集）。机器人实测为 `"7"`。

### 4.5 默认身份组 ID 实测

`1`=普通成员(全体)，`2`=超级管理员，`4`=频道主，`5`=子频道/版块管理员，`6`=访客；自定义身份组从其他 ID 段分配（本 guild 出现过 11、14）。

## 五、实测错误码速查

| code / err_code | 场景 | 含义与处理建议 |
|------|------|----------------|
| 505007 | 文字子频道调 online_nums | 仅音频/直播子频道支持 |
| 11263 / 40022001 | 用分组 id 当子频道 | 「频道不存在」，过滤 type=4 |
| 620003 | 重复 PUT 同一表态 | 幂等处理或先 DELETE |
| 302018 | 创建日程字段名错 | 必须用 `remind_type` |
| 304023 / 40034120 | 无 msg_id 的主动消息 | 进入安全审核，响应体含 audit_id；需订阅 MESSAGE_AUDIT(1<<27) 拿结果 |
| 50015001 | 删除日程 | 平台侧「系统繁忙」，重试无效，属已知故障 |
| 501002 / 501006 | 公告创建 | 废弃接口 / 条件不足 |
| 305101 | 创建私信会话 | guild 未开通频道私信 |
| 200010 | 创建子频道 | 机器人资质不足 |
| HTTP 204 / 200 {} | 多数删除类操作 | **响应体为空**，JSON 解析必须容忍空串/空对象 |

## 六、对 qq-bot 库的开发建议

### 6.1 websocket.rs 分发层（优先级最高）

现状：dispatch 只认 `C2C_MESSAGE_CREATE` 且要求 `message_type == 0`，其余全部丢弃。
建议：

1. 把硬编码的 `if t == "C2C_MESSAGE_CREATE"` 改为按事件类型分发表；
2. 新增事件类型：`MESSAGE_CREATE`（频道）、`AT_MESSAGE_CREATE`(公域兼容)、`GROUP_AT_MESSAGE_CREATE`（群@，当前也被丢弃！）、`DIRECT_MESSAGE_CREATE`、`GUILD_CREATE/UPDATE/DELETE`、`CHANNEL_CREATE/UPDATE/DELETE`；
3. `WebSocketHandler` trait 增加通用回调和/或按场景细分回调；现有 `MessageEvent` 结构偏 C2C，频道事件应建独立类型（author 无 user_openid、多 member/guild_id/channel_id）。

### 6.2 新增 guild 模块（REST）

按验证总表分两档实现，第一档即可支撑主流程：

- 第一档：get_me / get_guilds / get_guild / get_channels / get_channel / patch_channel / get_members(分页) / get_member / get_roles / get_role_members / get_channel_user_permissions / get_channel_role_permissions / get_api_permission / send_channel_message(passive: msg_id+msg_seq) / recall_message / get_pins / add_pin / remove_pin
- 第二档：reactions 三件套 / schedules(create/list/delete，注意 null 与毫秒时间戳) / roles 生命周期（解析 `role_id`）/ online_nums（校验 type）/ api_permission / message_setting

实现注意：

- 大量 204/`{}` 空体响应，`parse_response` 泛型需提供 `()` 或空对象路径；
- 所有 ID 都是字符串，权限位图是十进制字符串；
- `GET .../messages/{mid}` 响应有外层包装 `{message: ...}`。

### 6.3 业务层频控与时限（文档值，供设计参考）

- 被动回复 msg_id 有效期 **5 分钟**，同一 msg_id 最多回复 5 次，msg_seq 去重；
- 主动消息需审核通道 + 月度配额，第一版不建议依赖；
- 撤回窗口 2 分钟（仅自己的消息；管理员另可撤回成员消息，其 id 来自事件 `d.id`）；
- 精华每子频道上限 20 条。

### 6.4 遗留事项

- ⚠️ 测试残留：「饥荒助手内测」→日程子频道内有一条“接口验证测试日程”（平台删除接口故障无法清理），请在 QQ 客户端手动删除；
- PATCH roles、MESSAGE_AUDIT 事件流、成员增删身份组(PUT/DELETE .../roles/{rid})、禁言系列、踢人未验证（影响他人状态，留待真实需求出现再做）。


---

## 七、勘误与补充（2026-08-26 晚，SDK 联调期间追加）

| 发现 | 说明 | 库内处理 |
|------|------|----------|
| roles 接口 `hoist` 为整数 | `GET /guilds/{gid}/roles` 返回 `"hoist":1/0` 而非布尔，严格反序列化失败 | `de::opt_lenient_bool` 兼容布尔/0/1 |
| `/users/@me` 偶发 50015009 | 平台侧「系统繁忙」HTTP 500，重试成功 | 无需处理，调用方重试 |
| 文字子频道 online_nums 报文 | `{"code":505007,"message":"requested channel is not audio or live channel"}` | 与 §五 预期一致 |

---

## 八、单聊 markdown（msg_type=2）实测（2026-09-09，push-architecture 批次三前置）

> 验证方式：qq-bot CLI `send-markdown` 真实发送（AppID 19035***589，C2C 私聊 openid 为 my-wx78 现网接收人）。
> 目的：确定 render-hub QQ 渲染器（`PayloadKind::QqMarkdown`）的能力档次（DESIGN 未决事项 #1）。

| 试验 | 结果 | 结论 |
|------|------|------|
| 原生 markdown（标题+粗体） | ✅ 发送成功（HTTP OK，返回 message_id） | **原生 markdown 可用于私聊主动消息** |
| 原生 markdown（标题+列表+链接+引用） | ✅ 发送成功 | 列表/链接/引用语法均可通过 API 层 |
| 模板 markdown（`--template`） | ❌ 400 `40034124 markdown消息参数错误` | 该机器人**未申请 markdown 模板**；模板路线不可用（除非管理端申请，暂不需要） |
| 文本直发（msg_type=0 对照） | ✅ 发送成功 | 主动文本同样可用（未触发安全审核） |

**render-hub 定档结论**：QQ 渲染器以 **原生 markdown（`payload_kind=QqMarkdown`）为主 payload、`Text` 为 fallback**；模板路线（`--template`）在当前机器人资质下不可用。30_000 字节上限仍是占位值（QQ 侧未见公开硬限，保持 render-hub 现有注释与降级保护）。

**配额提醒**：以上均为主动消息（无 msg_id）。多次主动私聊未触发审核拦截，但平台月度配额消耗属实——推送类消息建议维持低频（现有 DST/新闻节奏可接受）。
