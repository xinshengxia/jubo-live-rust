# 虎牙弹幕迁移说明（danmaku-huya）

模块：src/danmaku_huya.rs　测试：tests/huya_danmaku.rs
状态日期：2026-10-29
结论：Tars 编解码、订阅帧、心跳帧、V1/V2 推送解析与聊天事件已实现并有离线合成测试；
模块工作流阶段未做真实平台联调（本工程测试默认不联网）；合并后的匿名实测见 §7
（2026-10-29，15 秒、1 条聊天；长时与礼物事件未覆盖）。

## 1. 协议范围

虎牙 Web 弹幕（旧 C# 工程 src/Danmaku/danmaku-9-huya.cs 实测行为）：

- 传输：匿名 WebSocket wss://cdnws.api.huya.com。**无路径、无 query、无子协议、无签名**；
  wsSecret 只属于推流地址，与弹幕无关。
- 载荷：腾讯 Tars/JCE 二进制（不是 protobuf）。没有 varint，写入器使用类型窄化；
  BYTE_ARRAY 是两个头字节（BYTES 头 + INT8 tag0 占位 + INT32 长度）。
- 订阅：cmd 16（RegisterGroupReq），组名 live:<lChannelId>、chat:<lChannelId>；
  组名必须用 lChannelId，用主播 yyuid 时服务器静默回空组。cmd 17 回显实际加入的组。
- 推送：cmd 7（V1）与 cmd 22（V2）两条通道；V2 的 tag1 是
  list<{iUri tag0, sMsg tag1, lMsgId tag2}>，每个元素是独立 Tars 结构体。
- 心跳：cmd 3，内容为 4 字节大端总长 + WUP 请求（onlineui / OnUserHeartBeat），
  旧实现 60 秒一次，服务端回 cmd 4 确认；连续两次 15 秒内无确认则主动重连。
- 聊天：cmd 7/22 里的 uri 1400 MessageNotice：
  tag0 tUserInfo{tag2 sNickName, tag4 sAvatarUrl, tag5 iNobleLevel}、
  tag3 sContent、tag5 ContentFormat{tag0 iFontColor}。
  读完 tUserInfo 必须吃掉 STRUCTEND，否则外层 tag3 会先命中 tUserInfo 内部的 tag3。
- 事件映射：只产出 DanmakuEvent::Message（聊天）；mode 固定 Scroll、font_size 固定 25
  （旧实现同样固定），color 取 ContentFormat.tag0 & 0xFFFFFF，缺省白色；id 为空串。
  cmd 17 回显会额外产出一条 DanmakuEvent::Status（订阅成功/空组）。
- **虎牙没有 SC**：官方“超级弹幕”内测且仅限受邀主播，141 个结构体 IDL 里没有付费留言
  消息类型；旧实现明确拒做“大额礼物升格 SC”。本模块同样不产出 SuperChat，
  礼物（6501）、进场（6110/6111）、在线数（8006）等 uri 只解析不渲染。

## 2. Tars/JCE 子集与边界

支持的类型：Zero / Int8 / Int16 / Int32 / Int64 / Float / Double / String1 / String4 /
Bytes / Struct / List / Map。读写器都做了边界检查，畸形输入返回 anyhow::Error：

| 边界 | 上限 |
| --- | --- |
| 单帧线上字节 | MAX_WIRE_BYTES = 2 MiB（与 danmaku 模块一致） |
| 单帧事件数 | MAX_EVENTS_PER_FRAME = 1024 |
| 文本/用户名字符 | MAX_TEXT_CHARS = 512 |
| Tars 嵌套深度 | 64（旧实现 MaxDepth 相同） |
| Tars 容器元素数 | 4096 |
| 主动解析的字符串字节 | 64 KiB |
| 房间页/缓存响应 | 2 MiB |

拒绝的畸形输入（测试覆盖）：空帧、截断帧（逐前缀）、超大声明长度、未知类型、
列表元素数超过上限、容器数超过剩余字节、缺少 BYTES tag0 占位头、
结构体缺少 STRUCTEND、非法 UTF-8（正文与昵称）、超深嵌套。
字符串一律做 UTF-8 校验，不做替换字符“将就”解析。

## 3. 已离线验证（tests/huya_danmaku.rs + 模块单测）

- Tars 写入器逐字节复现旧实现回归向量（类型窄化 + 两字节 tag）。
- **订阅帧 cmd 16 逐字节复现**旧实现实测报文
  （房间 333003 的公开频道 id，向量来自旧工程 tests/DanmakuTests.cs）。
- **心跳帧 cmd 3 逐字节复现**同上实测报文（含 WUP 嵌套、4 字节大端预置长度）。
- MessageNotice 用旧实现手工拼接的十六进制向量解析：正文/昵称/颜色/字号通过。
- V1 与 V2 推送各自解出聊天事件；非 1400 uri 被忽略且不伪造 SuperChat。
- tUserInfo 缺失时仍能取正文；空正文按“无可渲染事件”丢弃而不是报错。
- 正文/昵称超过 512 字符被截断；非法 UTF-8/截断/超长/超深输入全部返回 Err。
- cmd 17 订阅成功与空组两种回显都映射为 Status；cmd 4 心跳确认与 cmd 0 空包不产事件。
- 单帧事件数超 1024 返回 Err。
- 房间号解析：房间页 / 移动页 / mp 缓存三级优先级（合成页面字符串单测）。
- 重连退避函数上限 2 秒。

测试命令（本副本根目录）：

~~~
pwsh -NoProfile -File scripts/Test.ps1 -ToolchainRoot '<isolated-rust-root>'
pwsh -NoProfile -File scripts/Invoke-Cargo.ps1 -ToolchainRoot '<isolated-rust-root>' test --locked --offline --test huya_danmaku
~~~

## 4. 未实网验证 / 已知限制（live-untested / TODO）

- **模块工作流阶段未做实网联调**；合并后的匿名实测见 §7（15 秒连接、1 条聊天），
  长时、真实房间页解析、礼物/进场仍不证明线上可用。
- 房间解析依赖虎牙页面里 "yyid"/"uid"/"liveChannel"/lChannelId 等字段；
  页面结构变化会返回可操作错误（不猜 id）。未验证反爬、地区、登录态差异。
- 1.0.3 已迁移原版 39 个默认表情及 5 条补充/覆盖映射（40 个不同代码），
  连接前写入现有渲染字典缓存，图片按需下载。动态 WUP 表情包仍未实现；
  未知/付费/房间专属包保持文字回退，不猜测图片地址。
- 礼物/进场/在线数事件未建模；因虎牙无 SC，旧实现的“未知 uri 落盘诊断”也没有迁移
  （新工程不写用户数据目录）。
- 贵族等级 iNobleLevel 被解析后丢弃：DanmakuMessage 没有对应字段，不能塞进别的字段冒充。
- 心跳缺确认策略按旧实现迁移（15 秒 x 2 次），但真实服务器的确认延迟只在旧工程实测过，
  本工程未复测。
- 重连策略与旧实现不同：旧实现无限重连（3*attempt 秒，上限 30 秒），
  本工程按验收要求改为**最多 3 次、退避 500ms/1s、上限 2 秒**，用尽后返回 Err。
- 未发送的 cmd 1（RegisterReq）、cmd 2 WUP 响应只做“忽略”处理，没有建模。
- 未处理 WebSocket 分片消息：依赖 tungstenite 自动拼装（已设置 2 MiB 单消息上限）。

## 5. 上游参考与许可

- 协议/字段来源：本仓库旧 C# 工程 src/Danmaku/danmaku-9-huya.cs
  （CommandRegisterGroup / CommandHeartbeat / HandlePush / HandlePushV2 /
  ParseMessageNotice / Dispatch）与其回归测试 tests/DanmakuTests.cs；
  决策记录 docs/decisions/003-虎牙-tars.md、004-弹幕连接与窗口恢复.md。
  旧工程与本 Rust 工程同为 GPL-3.0 且已保留原 LICENSE：属于同一项目的迁移，
  **不需要新增第三方许可原文**。
- 旧 C# 文件头注释未声明独立版权或许可：只有工程内说明（单文件约束解除、
  三个反直觉协议坑、Tars 规则），因此没有需要单独保留的上游版权行。
- 需要在 THIRD_PARTY_NOTICES.md 增补/确认的条目（由 Lead 统一改，本工作流不改该文件）：
  1. 虎牙协议研究参考（仅消息号/订阅/回包路径对照，未复制代码）：
     hwenjie/huya_danmu、BacooTang/huya-danmu、biliup-start/VideoPlayer。
     旧工程 THIRD_PARTY_NOTICES.md 已把它们列为“协议与数据研究参考”，
     Rust 工程当前清单还没有这一条；建议补一条文字致谢（未复制代码，无需许可原文）。
  2. 腾讯 Tars/JCE 是协议格式来源（Tencent/Tars，上游 BSD-3-Clause）。
     本实现按旧实现实测行为编写，没有复制 Tars 源码，因此不强制附许可原文；
     如要在文档里点名协议规范，建议同时注明“仅格式参考”。
- docs/danmaku-protocol-sources.md 已更新为“Tars 子集与匿名 WS 接收已实现、离线测试覆盖”；
  实网覆盖范围以本文件 §7 的记录为准。

## 6. 状态清单

| 项目 | 状态 |
| --- | --- |
| Tars/JCE 子集读写与边界 | implemented + offline-tested |
| 订阅帧 cmd 16 / 心跳帧 cmd 3 | implemented + offline-tested（逐字节对齐旧实现） |
| cmd 7 / cmd 22 推送与 uri 1400 聊天解析 | implemented + offline-tested |
| 匿名 WSS 连接、订阅、心跳、500ms 读轮询、有界重连 | implemented，**live-untested** |
| 房间号页面解析 | implemented，仅合成页面单测，**live-untested** |
| 订阅回显/错误 Status 事件 | implemented + offline-tested |
| 默认与已知补充表情 | 1.0.3 已接入渲染字典；40 个不同代码 |
| 动态表情包 WUP、礼物/进场事件、SC | TODO / out of scope（虎牙无 SC） |
| 真平台联调与线上验收 | TODO（需联网环境，由 Lead 安排） |
## 7. Lead 合并后的匿名实测（2026-10-29）

- 目标 `huya:660000`，离线工具 `danmaku-probe` 运行 15 秒：订阅 `chat:1346609715` 成功，
  收到 1 条真实聊天消息，`connect` 无重连、无错误。
- 同环境中还验证了虎牙匿名取流 + 真实标题，以及真实直播流经本机 FLV 中转（10 秒 596 KB、133 个完整 tag）。
- 仍未覆盖：长时稳定性、礼物/进场事件、表情包、心跳确认延迟；默认测试套件仍然完全不联网。
- 环境注意：本机 TUN 代理的 fake-IP（198.18.0.0/15）曾被地址白名单拒绝；现在弹幕/中转允许该段，
  由 TLS 主机名校验保证对端仍是固定平台域名。
