# 抖音弹幕迁移（workstream: danmaku-douyin）

模块：`src/danmaku_douyin.rs`（独立模块，不修改 `src/danmaku.rs`；由 Lead 负责接 dispatch）。
测试：`tests/douyin_danmaku.rs`（离线合成数据，不联网、不打开播放器、不读真实用户数据）；
`tests/douyin_live_acceptance.rs`（**默认 ignored**，仅显式 opt-in 时访问真实网络）。

## 公共契约（与占位 stub 一致）

```rust
pub fn decode(frame: &[u8]) -> anyhow::Result<Vec<crate::danmaku::DanmakuEvent>>
pub fn connect(target: &crate::core::Target, sender: SyncSender<DanmakuEvent>, cancellation: Cancellation) -> anyhow::Result<()>
```

为支持离线测试，模块额外公开了纯函数与编解码件（签名、percent-encode、join URL、PushFrame、ACK/心跳包、Response 检查、房间页解析/失败分类、心跳 watch、重连退避等），见模块内 `pub` 项。

## 已实现且离线测试覆盖

| 能力 | 说明 | 离线测试 |
|---|---|---|
| DouyinSign（frontierSign / X-Bogus） | MD5 两跳 + RC4 + 自定义字母表 base64，纯算法；counter 0..=63 自增、随机位每次重算不缓存 | 上游公开向量 `6pt0VC0aRZDP7n07` / `fD+evFuzasjOC8Uu` 逐字节一致；非法 stub 拒绝 |
| percent-encode | 与 .NET `Uri.EscapeDataString` 语义一致（仅 `A-Z a-z 0-9 - . _ ~` 保留） | 签名中的 `+` / `/` 必须转成 `%2B` / `%2F`；空格、UTF-8 中文、保留字符 |
| join URL | 按 C# 逐项拼接（room_id、user_unique_id、cursor、internal_ext、UA 指纹、signature）；host/room/uid/signature 均校验，防注入 | 固定输入断言参数与编码；非法 host/房间号/签名被拒；三个已知 webcast host 可用 |
| PushFrame 编解码 | field 8 = payload、field 7 = payloadType、field 5 = headers（key/value）、field 2 = logId；未知字段跳过 | 编码-解析往返；空 payload/零 logId 省略规则 |
| gzip 有界解压 | 复用 `danmaku::MAX_WIRE_BYTES` / `MAX_EXPANDED_BYTES` 预算；非 gzip 容错按裸 protobuf 处理 | 8 MiB+1 解压炸弹被拒 |
| 最小 protobuf reader/writer | 只支持 wire type 0/1/2/5，varint 溢出/截断、长度越界、字段号非法、字段数上限（4096）都有界；writer 用于 hb/ack | 截断、wire type 3、field 0、varint 溢出、字段数超限全部 `Err` 不 panic |
| Response/Message/Chat 解码 | Response.field1 → Message(method,payload) → 仅 `WebcastChatMessage` 转 `DanmakuEvent::Message`；Response.field5 取 internalExt | gzip/裸包两种 chat 样本；长文本截断到 `MAX_TEXT_CHARS`；空内容忽略；事件数上限 |
| 心跳/ACK 包 | hb = `3A 02 68 62`；ack = PushFrame{logId, "ack", internalExt} | 逐字节断言；ack 解析往返 |
| 心跳 watch | ACK 超时上限 5 s；连续 2 次缺失才重连；迟到 ACK 清零 | 纯逻辑时钟测试 |
| 重连 | 首次连接后最多 3 次重连；退避 3/6/9 s，封顶 30 s | 常量与退避断言 |
| WebSocket 生命周期接线 | join URL → TLS WS 握手（每 host 依次尝试 3 个）→ 循环：10 s 心跳、5 s ACK watch、收帧解码、回 ACK、cancel 500 ms 内退出 | 纯逻辑离线测试 + **真实匿名链路手工验收（见下）** |
| 房间页解析 | 线性扫描 `self.__pace_f.push([...])` / `self.__next_f.push([...])`，去 flight 前缀，取 `state.roomStore.roomInfo.room` 的 id_str/status/title/owner 与 `user_unique_id` | 合成 fixture：对象/数组负载、两种 marker、转义 uid、空壳页、验证码页 |
| 失败分类 | `RoomFailure{LoginRequired, RiskControl, Malformed}` + `classify_room_response`（HTTP 状态/空 body/JSON-only）+ `parse_room_page_classified`（空壳页/验证码页/格式变化）；验证码判定带 <64 KiB 大小护栏（现网正常页约 1 MB 且含 verifyFp 字样） | 合成用例覆盖 401/403、429、200+空 JSON、空壳页、验证码页、格式变化、大页面不误判 |
| ttwid cookie | 从 `Set-Cookie` 提取 ttwid 供 WS 握手 | 纯解析测试 |

## 真实链路验收（2026-10-29，匿名、无用户凭据）

环境：Windows 本机、TUN 代理 fake-IP；全部请求匿名，未使用浏览器 profile、LiveHub 数据目录或真实 Cookie；测试前后 `GetForegroundWindow` 同一窗口，未抢焦点。证据见本机 notes（不随发布包分发）。

**结论矩阵**

| 环节 | 结论 | 说明 |
|---|---|---|
| 房间页解析 | ✅ 通过 | 公开直播间 `live.douyin.com/<web_rid>` 匿名返回约 1.2 MB 页面；解析出 id_str/status/title/owner/user_unique_id |
| 取流（resolve） | ✅ 通过 | 返回 HTTPS FLV 直链 + 真实标题；跟随 CDN 302 后 8 s 读到 11.7 MB、`Content-Type: video/x-flv`、首 3 字节 `FLV` |
| 弹幕 WebSocket 握手 | ✅ 通过 | 匿名 join（signature/join URL）被接受，状态"已连接 · <owner>"，无 401/拒绝 |
| 心跳/ACK | ✅ 通过 | 25 s 观察期无"连续两次心跳没回包"、无断线重连（按代码若心跳无回包会在约 25 s 内断开） |
| 聊天事件 | ✅ 通过 | 活跃房间 25 s 收到 136 条 `WebcastChatMessage`；另一房间 30 s 收到 67 条 |
| 下播房间 | ✅ 通过 | status=4 时 resolve 明确报"未开播"，弹幕连接后提示"房间未开播，仍在等待弹幕"，正常取消 |
| 不存在/空壳页 | ✅ 通过 | 报"空响应或空壳页（房间不存在与临时限流页面相同，无法区分）"，重连 3 次后停止（3/6/9 s 退避） |
| 登录态 / 真人验证流程 | ❌ 未迁移 | 浏览器扫码/滑块、cookie 续期未实现；见"凭据导入" |
| GUI 覆盖层实机渲染 | ❌ 未验证 | 需 playback/overlay workstream 的窗口验收；本 workstream 只验收弹幕数据链路 |
| 长时间稳定性 / 断流重连 | ❌ 未验证 | 见 relay-stability workstream |

**复现命令**（隔离工具链 + ASCII target 目录；必须用 `--data-dir` 指向临时/独立目录，避免读取用户真实数据根）：

```powershell
$exe = '<ascii-target>/debug/live-hub-rust.exe'
$data = Join-Path $env:TEMP ('live-hub-douyin-' + [guid]::NewGuid())
& $exe --data-dir $data resolve douyin:<公开直播间 web_rid>
& $exe --data-dir $data danmaku-probe douyin:<公开直播间 web_rid> --seconds 20
```

有网络时也可跑 ignored 验收测试：

```powershell
$env:LIVE_HUB_LIVE_ACCEPTANCE='1'; $env:LIVE_HUB_DOUYIN_ROOM='<web_rid>'
pwsh -NoProfile -File scripts/Invoke-Cargo.ps1 -ToolchainRoot '<root>' -TargetDirectory '<ascii target>' \
  test --locked --offline --test douyin_live_acceptance -- --ignored --nocapture
```

**本轮真实链路暴露并修复的问题**

1. **本机 TUN fake-IP 被地址策略误杀**：`platforms.rs::endpoint_client` 只接受 `is_public` 地址，本机 DNS 对 live.douyin.com 返回 198.18.0.0/15 假 IP，过滤后为空 → 报"平台域名没有可用的公网地址"。修复：与弹幕/relay 既有策略对齐，允许 TUN fake-IP（仅对已通过固定主机白名单与 TLS 主机名校验的请求生效），并加离线回归。**这不是抖音风控，纯本机网络环境问题。**
2. **实测 CDN 家族缺口**：真实房间的 FLV host 是 `pull-flv-*.douyinliving.com`（HTTPS 边返回 video/x-flv），旧版固定 CDN 家族白名单会拒绝未列出的家族。1.0.6 起媒体地址不再要求固定家族：语法/凭据/私网/本机名策略保留，未知公网主机解析后必须为公网或 TUN fake-IP（中继逐跳再钉扎），并保留具体拒绝原因上抛；初始地址保留原协议，`https→http` 跳转降级会被拒绝。
3. **反爬形态识别**：非浏览器 UA 访问房间页得到 `HTTP 200 + Content-Type: application/json + 0 字节`（不是 403/验证码）。修复：`classify_room_response` 把 2xx 空 body / JSON-only 短 body 归类为 `RiskControl`，给出可操作文案而不是"页面改版"。
4. **验证码启发式误判**：原判定 `page.contains("verify")` 在现网正常页（含 verifyFp 等脚本名）也会命中。修复：加 <64 KiB 大小护栏 + 已知中间页标记（`验证码中间页`/`sec_sdk_build`），并用合成大页面回归。
5. **不存在房间不可区分**：空壳页与"房间不存在"同模板（C# 决策 010 已记录），统一归类 `RiskControl` 并在文案里说明"无法区分"，不谎称页面格式变化。

**为什么匿名可用，而前几轮以为必须登录**：公开搜索接口（`www.douyin.com/aweme/v1/web/discover/search/`）确实要求登录/验证；但**房间页与 webcast 弹幕链路不需要登录**。之前未打通的主因是本机地址策略（问题 1）与 CDN 白名单（问题 2），不是账号。

## 凭据导入（可选，匿名链路通常不需要）

当出现真人验证/风控或需要登录专属内容时，用**显式导入**（绝不自动读取浏览器 profile/Cookie）：

```powershell
# Cookie 文件：Cookie 头字符串或 Netscape cookies.txt（用户自行导出）
& $exe --data-dir <独立数据目录> auth import douyin <cookie.txt>
& $exe --data-dir <独立数据目录> auth status
& $exe --data-dir <独立数据目录> auth clear douyin
```

- 凭据用 Windows DPAPI 加密存于独立数据目录的 `credentials`，明文不落盘、不写日志、不打印。
- 域作用域：只对 `douyin.com` 及其子域、仅标准 HTTPS/WSS 源生效（`danmaku_session.rs` 的 scoped cookie 规则）；不会发往 CDN 或其他平台。
- 导入后弹幕/取流/中转/播放都会按域自动携带（`tests/authenticated_danmaku.rs` 覆盖域作用域与错误处理）；1.0.6 起起播链支持「已有专属会话静默刷新一次」与账号页「抖音网页验证」可见窗口（用户自己完成滑块/点选，程序用本应用专属 profile 的 CDP 捕获 ttwid）；仍不读取日常浏览器登录态，也不绕过验证。

## 表情字典（已实现）

弹幕文本里的行内表情码（如 `[捂脸]`）是平台字典里的图片短码，不是 Unicode 字符。Rust 侧现在与 B站 共用同一条「字典 → 文本分段 → 远程图片」渲染路径：

- **请求**：连接抖音时后台线程请求一次 `GET https://www.douyin.com/aweme/v1/web/emoji/list/`，浏览器 User-Agent（与房间页一致）+ `Referer: https://www.douyin.com/`，`Accept: application/json, text/plain, */*`。未导入凭据时匿名可用（2026-11-01 本机实测 HTTP 200、`emoji_list` 214 条、约 118 KB）；若导入的凭据保留 `douyin.com` 域作用域（如 Netscape 导出），请求按域携带 Cookie；普通 `Cookie:` 头导入默认只绑定 `live.douyin.com`，不会发给 `www.douyin.com`。不写日志。
- **解析**：`emoji_list[].display_name`（`[捂脸]` 这类代码）映射到 `emoji_url.url_list` 中首个通过 URL 白名单的 HTTPS 地址（平台 CDN 家族，如 `douyinpic.com`）；空代码、超过 24 字符的代码、非白名单 URL 丢弃；最多 1024 条，响应上限 1 MiB。
- **缓存**：写入进程内表情缓存，空字典不覆盖旧字典；失败、超时或非 2xx 静默忽略（保留上一份字典），不影响弹幕连接。覆盖层按缓存代次刷新，换房间/换平台也会取到新字典。
- **解码**：图片走 `images::decode`（2 MiB 线大小、2048 px 边长、24 MiB 分配上限）。抖音表情实测为 **VP8L 无损 WebP，96×96**，由 `image` + `image-webp` 解码；任一步失败时该表情退回方括号文字，不影响其它弹幕。
- **验证**：离线渲染证据 + 本机真实匿名接口验收见 `docs/validation.md` 的 1.0.1 小节。

## 明确未实现 / 未验证（不要当成已实现）

1. **登录/风控交互的部分迁移**：1.0.6 已迁移起播链所需部分——房间页多路回退（pinned IPv4/系统解析/只读 Windows 系统代理）、专属 profile 隐藏浏览器静默刷新、账号页可见「抖音网页验证」窗口、中继验证等待态；未实机验证真实受验证房间。仍不复用 C# 的 `douyin-verify-profile`/登录态，不读取日常浏览器，不自动完成滑块。
2. **未验证 GUI 覆盖层实机渲染**：本 workstream 验收的是弹幕数据链路（事件数/握手/心跳）；透明窗口渲染、跟随、SC 卡片属 overlay/playback workstream。
3. **未做长时间稳定性**：25–30 s 观察窗口；跨小时、断流重连、多房间并发属 relay-stability workstream。
4. **事件类型部分迁移**：`WebcastChatMessage` 之外，1.0.6 新增 `WebcastGiftMessage` → 礼物、`WebcastMemberMessage` → 进场、`WebcastLikeMessage` → 点赞统计行（映射到既有 `DanmakuEvent::Live`，未扩展契约），字段布局按两个独立 MIT 公开实现交叉核实。**真实平台验收（匿名）**：直播间 46595453118 90s 收到进场 383、点赞 33、聊天 165；再次 120s 收到进场 458、点赞 44；另一房间 120s 收到进场 140、点赞 2——Enter/Like 已实机验证；**礼物事件在约 8 分钟、5 个房间窗口内未出现**，Gift 适配仍只有协议来源+合成测试。关注/分享/人数/大表情聊天/电商/`PaidLive*` 仍被有界跳过。抖音**没有 SC 等价物**（C# 侧已核对 610 条 proto message）。
5. **平台自定义表情包**：只用公开 `emoji/list` 返回的表情；用户自定义表情包、付费表情与需要登录权限的表情未验证。
6. **user_unique_id 兜底**：页面里找不到时用 `76` + 17 位随机数字（与 C# 一致）；真实页面均能提取到页面内嵌 uid，兜底路径未单独实网验证。
7. **状态判断仅提示**：房间 `status != 2` 时仍保持连接等待（与 C# 弹幕行为一致），状态只用于状态栏文案，不用于播放判断。
8. **直播状态/取流**由 `platforms.rs` 负责；本轮已实测 resolve 可用（见上），但播放器窗口/potplayer/mpv 解码未在本 workstream 验收。

## 协议要点（迁移依据）

- `signature = frontierSign({"X-MS-STUB": md5(join 参数块)})["X-Bogus"]`：MD5(hex stub) → 10 字节 payload（counter/常量/两个 md5 的第 14、15 字节/随机/校验和）→ 2 字节头（`0x40|0x10`、keyRandom）→ RC4(key=[keyRandom]) → 自定义 base64（无 padding）。签名含 `+`/`/`，join URL 里必须 percent-encode，否则握手直接失败。
- WS 二进制帧 = `PushFrame`；payload 只有**一层** gzip；解出 `Response`；`Response.field1` 是重复的 `Message`（field1=method，field2=payload），`field5`=internalExt。
- 心跳：每 10 s 发 `PushFrame{payload_type:"hb"}`（`3A 02 68 62`）；回包也是 payload_type=hb；5 s 未回记一次 miss，连续 2 次重连。
- ACK：收到带 `logId`（field2）且 Response 带 `internalExt`（field5）的帧后，回 `PushFrame{log_id, payload_type:"ack", payload:internalExt}`。
- join URL 参数、`cursor`/`internal_ext` 格式、三个 webcast host 均按 C# 逐项对齐；实测三个 host 中首个可用即连接成功。
- 房间页：`self.__pace_f.push([<序号>,<JS 字符串字面量>])`；payload 形如 `d:["$","$L12",null,{"state":{"roomStore":{"roomInfo":{"room":{...}}}}}]`（数组里找带 `state` 的对象）；正常页约 1–1.5 MB，含 `user_unique_id` 内嵌 uid 与 `stream_url.flv_pull_url` 四档清晰度。

## 许可与上游

- **DouyinSign 适配来源**：[jwwsjlm/douyinLive](https://github.com/jwwsjlm/douyinLive)，MIT，Copyright (c) 2025 今晚打打。
  - 签名实现参考：`https://github.com/jwwsjlm/douyinLive/blob/0e506f3cf935beede946296602af2e52d57ecea2/internal/webcastsign/native.go`
  - 公开测试向量：同 commit 的 `internal/webcastsign/native_test.go`（C# 工程 `tests/DanmakuTests.cs` 的 `TestDouyinSign` 记录的即这两组向量）
  - 本工程按行为重写为 Rust，**没有**引入 Go/JS 运行时代码，**没有**复制上游源码；仅采用其公开算法描述与测试向量。
- 协议字段布局参考：原 C# 工程 `src/Danmaku/danmaku-10-douyin.cs`、`danmaku-17-connection.cs` 的线上观测笔记（本仓库不复用其代码与数据）。
- 失败分类与反爬/限流判据参考 C# 决策记录：`docs/decisions/002-抖音-ipv6-验证码中间页.md`、`010-抖音-限流空壳页与重试.md`（只采用结论，不复用代码/页面数据）。

### 需要 Lead 在许可/声明文件中增补（不属于本 workstream 的归属文件）

1. `licenses/douyinLive-LICENSE.txt`：原文直接取自 C# 工程同名文件（1069 字节，SHA-256 `792d236d726b8bd52d4958cbbf40f160d19491c94915c4ef4416411f5051b008`），内容为 MIT "Copyright (c) 2025 今晚打打"。
2. `THIRD_PARTY_NOTICES.md`：新增 "### douyinLive — MIT" 小节，写明项目链接、版权、使用范围（`src/danmaku_douyin.rs` 的 DouyinSign 适配 + `tests/douyin_danmaku.rs` 的公开向量），并保留原文链接（`licenses/douyinLive-LICENSE.txt`）。
3. `docs/danmaku-protocol-sources.md`：抖音段落目前写的是"Live signing, ACKs, heartbeat, login, gifts, and other event types are not enabled"，与本次迁移不符，建议改为：Chat 解码 + 签名/join URL/ACK/心跳已迁移并有离线测试，**匿名真实链路已手工验收（2026-10-29）**；礼物等其他事件仍不转发；未迁移登录/风控。
4. 可选：`licenses/upstream/SOURCES.json` 若继续记录上游快照，可加 `jwwsjlm/douyinLive`、revision `0e506f3cf935beede946296602af2e52d57ecea2`、LICENSE 原文 URL `https://raw.githubusercontent.com/jwwsjlm/douyinLive/0e506f3cf935beede946296602af2e52d57ecea2/LICENSE` 与对应 SHA-256（若该 URL 文本与 C# 工程内文件一致，可直接复用上面的哈希）。

## 测试命令

在副本根目录（本机工具链不在 PATH，必须用包装脚本）：

```powershell
pwsh -NoProfile -File scripts/Test.ps1 -ToolchainRoot '<isolated-rust-root>' -TargetDirectory '<ascii-target>'
```

本 workstream 的用例可单独跑：

```powershell
pwsh -NoProfile -File scripts/Invoke-Cargo.ps1 -ToolchainRoot '<isolated-rust-root>' -TargetDirectory '<ascii-target>' \
  test --locked --offline --test douyin_danmaku --test platform_contract
```

全部默认测试为离线合成数据；真实网络验收只在 `#[ignore]` 的 `tests/douyin_live_acceptance.rs` 或手工 `resolve`/`danmaku-probe` 中进行。

本轮（2026-10-29）完整默认套件：**186 passed / 0 failed / 3 ignored**（基线 181/0/2；新增用例：`douyin_danmaku` +3 失败分类、`platforms` +1 fake-IP 单测、`platform_contract` +1 实测 CDN，另新增 ignored 真网验收 `douyin_live_acceptance`）。ignored 真网验收已实际执行通过（resolved title + 已连接 + 20 s 41 条聊天）。

## 后续建议

- 已可宣布"抖音匿名解析 + 弹幕可用"，但**不要**宣布"抖音播放器窗口/覆盖层可用"：后者属 playback/overlay 验收。
- 风控 cookie 生命周期建议由 auth/平台适配 workstream 统一提供（显式传入 cookie，不在弹幕模块读写用户数据）；真人验证（扫码/滑块）若要产品化，需要单独设计"用户主动过验证 + 显式导入"流程。
- 若产品需要礼物/进场提示，需要先扩展 `DanmakuEvent`（新增 Gift 变体），再补 `WebcastGiftMessage` 字段布局调研；当前不猜测字段号。
- 长时间稳定性（跨小时心跳/断流重连）建议纳入 relay-stability 的 soak 测试，用 `danmaku-probe` 长时间窗口复测。


## 起播恢复链（1.0.6）

- 房间页读取按业务结果换路：pinned IPv4 → 系统解析 → Windows 系统代理（只读设置），最多 3 路、共享 deadline；200 验证码页/空壳页/格式变化页都会继续换路，明确下播立即终止。
- 验证恢复：解析返回验证要求时，专属 profile 隐藏浏览器静默刷新一次（single-flight + 60 秒冷却 + 可取消），成功只重试当前房间一次；中继进入 `verification` 等待态（保持连接、不计失败预算）。账号页「抖音网页验证」提供用户正常完成的可见窗口，捕获 ttwid 后加密保存。
- 同房间状态/标题/头像：单飞 + 4 秒短复用；起播始终新取页面，不回放陈旧直链。FLV 画质键大小写不敏感。
- **真实平台验收（2026-11-07，匿名/TUN）**：真实直播间取流与中继成功（http 直链保留，5s 158 帧/1.6MB）；
  真实验证码窗口返回类型化错误；全新专属 profile 的隐藏浏览器在 25s 内识别「验证码中间页」并返回 false、不保存
  伪 Cookie（说明静默刷新需要持久 profile 已通过过一次验证）；profile ACL 实测仅 OWNER+SYSTEM。
- **仍未验证**：已通过验证的持久 profile 上的静默刷新成功、真实风控页里用户手动滑块的交互、系统代理模式
  （当前机器 ProxyEnable=0 + Mihomo TUN，切换模式会修改用户设置，未做）、真实礼物事件样本、物理声画与长时多窗口。
