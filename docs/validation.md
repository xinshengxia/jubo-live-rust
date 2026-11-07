# Validation report

更新：2026-11-07。本报告区分真实验证与合成回归；不包含账号、Cookie、宿主路径或截图。

## 1.0.7 本轮验证（2026-11-07）

### Rust（1.0.7）

- 全量离线测试：**746 passed / 0 failed / 31 ignored**（47 个常规测试目标 + 1 个 doctest 结果，48 条 test result）。命令：
  `scripts/Invoke-Cargo.ps1 -ToolchainRoot <toolchain> -TargetDirectory <ASCII target> test --locked --offline`；
  `check --all-targets --offline --locked` 退出 0。相对 1.0.6 基线 730/0/30 新增 16 个通过、1 个 ignored，均为本次新增测试。
- 新增覆盖（全部离线，不启动真实播放器/网络/用户数据）：
  - mpv 默认参数只追加 `--ao=openal`，不新增 `--video-sync`/`--framedrop`；显式 `--ao=`/`--audio-device=` 覆盖默认；`--no-audio`/`--audio=no`/`--aid=no` 与 `--` 之后不约束。
  - 子进程环境仅含 `ALSOFT_DRIVERS=wasapi` 与包内 `ALSOFT_CONF`，父进程环境不变；PotPlayer、非 OpenAL 与禁用音频路径 0 环境覆盖。
  - 包内 `assets/audio/openal-wasapi.ini` 缺失、超 16 KiB、非 UTF-8、重复/缺失/错误段均拒绝；能力检查要求真实 `Available audio outputs:` 表头 + 缩进 openal 行，缺 OpenAL 时在启动子进程前失败。
  - PotPlayer 计划不得出现任何 `--ao`/`--audio-device`/同步参数或 ALSOFT 环境（契约测试锁定）。
- 发布包 lite/source 的构建、manifest、许可与隐私门禁由发布任务单独验收。
- 未验证：真实 mpv `--ao=help` 的进程级输出与退出码（新增 ignored 验收测试可在发布验收时用真实 mpv 运行）、真实平台长时播放（本次修复依据用户此前两小时实播会话验证，不等于全新启动路径的端到端复测）；PotPlayer 未改动，其长时同步无独立证据。

### C#（1.1.8）

同一音频输出修复同步合入 C# 版；离线回归 231 passed / 0 failed / 12 skipped（243 用例，5919 断言），发布包与隐私验收见该版发布记录。


## 1.0.6 本轮验证（2026-11-07）

### Rust（1.0.6）

- 全量离线测试：**730 passed / 0 failed / 30 ignored**（48 个 test target，默认 GUI features）。命令：
  `scripts/Invoke-Cargo.ps1 -ToolchainRoot <toolchain> -TargetDirectory <ASCII target> test --offline --locked`；
  日志在本机验收记录。ignored 均为显式真实平台/性能/压力 harness（含本轮新增的
  `live_silent_refresh_acceptance`），不是静默跳过。
- 本轮新增/改写的合成回归（不访问网络、不启动浏览器/播放器，全部使用注入传输/DNS/假时钟）：
  - 抖音房间页业务失败换路：200 验证码页→备用路径正常、空壳重试、三路耗尽分类（验证码/限流/格式/网络）、
    验证码后系统代理优先、明确下播终止、白名单 https 重定向/循环/跨主机拒绝。
  - 系统代理配置解析与 bypass（host:port、http=/https=、<local>、通配符）；只读，不修改系统设置。
  - 同房间并发单飞、4 秒短复用、起播强制新取、凭据变化/主动失效、缓存 TTL。
  - FLV 画质键大小写不敏感与优先级；验证码判定收窄；正常房间页带 captcha SDK 仍可解析（回归）。
  - 起播验证恢复：只重试一次、刷新失败/取消不循环、二次验证保留可操作错误。
  - 中继验证等待态：保玩家连接、不消耗连续失败预算、退避后恢复；玩家断开时阻塞中的解析立即结束
    （新增集成测试，resolver 阻塞 3s，玩家 100ms 断开，连接 <1s 关闭且不写媒体）。
  - 媒体协议保留/降级拒绝/跨域头策略/未知公网 CDN 与私网拒绝。
  - 抖音富事件 Gift/Member/Like 合成 protobuf 解码、异常 payload 跳过、单帧预算。
- **真实平台验收（本机 Clash TUN 模式，匿名、未登录、未读用户凭据/profile；只读）**：
  - 真实抖音直播间 `46595453118`：`resolve` 成功（真实 FLV 直链为 **http**，协议未被强制升级）；
    `streaminfo` 经生产中继打开真实 CDN：HEVC 720x1280 约 15fps，5 秒采到 **158 帧 / 1,615,391 字节**、
    媒体时间跨度 7,092ms、仅 1 个初始异常标记。证明新公网 DNS 策略、协议保留、逐跳钉扎在真实 CDN 上可用。
  - 真实风控窗口：连续请求后同一 IP 的匿名房间页返回 6,297 字节验证码中间页（含 captcha 标记、无 roomStore），
    应用返回类型化「平台要求登录或网页验证」，未误判为下播/网络错误；同时另一直播间仍可正常取流。
  - 真实静默刷新（专属 profile、**全新无历史 Cookie**）：隐藏 Edge 打开真实直播间，25s 内页面标题为
    「验证码中间页」，仅有 `__ac_referer`/`s_v_web_id`、无 `ttwid`，函数按设计返回 false、不保存伪 Cookie、
    关闭自有浏览器（25.48s 内结束）。结论：静默刷新在**已通过过一次验证的持久专属 profile**上才有成功基础；
    全新 profile 只能走账号页可见窗口由用户完成滑块——这是人机验证的本意，本轮不绕过。
  - 验证 profile 目录 ACL：`icacls` 实测仅 `OWNER RIGHTS:(OI)(CI)(F)` 与 `NT AUTHORITY\SYSTEM:(OI)(CI)(F)`，
    无 Users/Everyone/Builtin；与 C# RestrictProfile 同级。
  - 真实抖音弹幕富事件（匿名 WebSocket）：直播间 `46595453118` 90s 收到聊天 165、进场 **383**、点赞 33；
    再次 120s 收到聊天 99、进场 **458**、点赞 44；另一房间 120s 收到进场 140、点赞 2。即
    `WebcastMemberMessage`→Enter、`WebcastLikeMessage`→Status 已在真实平台验证。**礼物事件**在约 8 分钟、
    5 个直播间窗口内未出现，故 Gift 适配仍只有协议来源+合成向量，未获真实样本。
  - 真实平台部分房间/部分时段弹幕握手仍返回「需要人工登录或网页验证」的类型化错误（另一房间同窗口正常），
    属平台侧风控差异，不应写成实现失败。
- Release 构建与包校验：`artifacts/release-1.0.6`，版本 1.0.6；包文件 **467 条 / 校验 469**，manifest 哈希一致、
  排序与必需许可/字体条目通过、**私人路径扫描 0 命中**；`live-hub-rust.exe --version` = 1.0.6，离线自检 **7/7**。
- 回退：`artifacts/release-1.0.5` 保留（466 文件，manifest 存在），`--version` = 1.0.5，离线自检 7/7；
  `artifacts/current` junction 已切换到 release-1.0.6，开始菜单目标解析正常。
- **仍未验证**：已通过验证的持久 profile 上静默刷新成功率（需要用户先完成一次滑块）、真实风控页里用户
  可见验证窗口的人工交互、真实系统代理模式（当前机器 ProxyEnable=0 + Mihomo TUN，切换系统代理模式属于
  修改用户设置，本轮未做；TUN 模式已实测）、真实礼物事件、物理声画与长时多窗口。

### C#（1.1.7）

- 从干净 HEAD `fe1990c` 一次 `Build-Hardened.ps1 -Headless` 构建（build ID `1.1.7+fe1990cd5777`，无 dirty 标记）。
- 离线测试：**228 passed / 0 failed / 12 skipped（240 用例），5815 条断言**；12 项为原生窗口/DPI 桌面检查
  （headless 明确跳过）。
- **真实 B站 ver3 线上验收（匿名、未登录，90 秒）**：公开房间 7734200，自行实现 WBI 签名取 getDanmuInfo，
  `protover=3` 连接 `wss://<host>/sub`；收到 **36 个 ver=3 brotli 帧**，用仓库 vendored 解码器经生产
  `BiliDanmakuClient.Feed` 路径逐帧解码：43/43 帧成功、36/36 ver3 帧解压成功（81–1228B → 77–3122B），
  解出 44 个内层 JSON 包（DANMU_MSG 10 等），生产事件数 10 与内层 DANMU_MSG 一致。证据：
  `output/bili-ver3-live-evidence/`（43 原始帧 + 逐帧哈希 + 解码摘要，不含弹幕文本/用户名/UID）。
- Brotli ver3 合成向量：4 用例/13 断言通过；认证包仍为 `protover=2`。
- 回退：`artifacts/rollback-1.1.6`（44 文件 / 29,826,244 字节，含 rollback-files.json 哈希清单）。
- **原生桌面/DPI 用例**：headless 套件跳过的 12 项本轮首次在交互桌面实际运行。完整套件 **235 passed /
  5 failed / 0 skipped**；其中 2 项时序敏感夹具超时单独重跑通过（1.1.6 与 1.1.7 均通过），**3 项在 1.1.6 回退
  程序上同样失败**（多倍率八页矩阵 60s 超时、重复 DPI 字体/滚动条 15s 超时、WM_DPICHANGED 288 建议矩形不一致），
  属既有桌面/DPI 夹具问题，非本轮改动回归；本轮未改布局/DPI 代码。
- **未验证**：登录态/洪峰/长时 ver3 帧型与 SC/礼物等；真实多显示器混合 DPI 物理环境与完整 GUI 交互。

### 清理

本轮按版本保留规则清理：Rust `release-1.0.4`（465 文件 / 37,271,439 字节）、C# `rollback-1.1.5`（43 文件 / 29,804,401 字节）
以及本轮临时 cargo target 目录，合计释放约 **17.8 GB 文件字节**；保留当前版本、上一版回退与用户数据。明细见本机验收记录。

## 0.3.0 本轮验证摘要

### 离线回归矩阵

- 构建：`cargo build --locked --offline`（默认 GUI）与 `--no-default-features` 均通过；两矩阵最终数字见下方 §0.3.0-matrix（冻结树，2026-10-31）。
- 新增/更新契约测试：overlay_resize_acceptance 6/6（resize/DPI/全屏保 progress）、overlay_shortcut_hint_acceptance 4/4（提示行开关与旧配置兼容）、avatars_contract 10/10、playback_hints_contract 11/11、hotkeys_contract 6/6、window_policy_contract 6/6、widget_gallery 2/2、danmaku_contract 12/12（新调度语义）、overlay_layout_evidence（新 SC API）。
- 基线对照：0.2.4 基线构建/回归 BUILD_TEST_EXIT=0（394 passed/0 failed/14 ignored，默认 features），见任务 `tmp/baseline-build-test.log`。

### UI 还原对照（离线渲染帧，非真实桌面）

- Rust `--ui-shot` 逐页离屏截图：8/8 页 2480×1440（=1240×720 逻辑 @200%），确定性可复现，前台采样 162–236 次从未抢前台（`capture-evidence.json`）。
- 原版真实窗口参照：8 页深/浅色 2480×1440 + 每页控件树（本机验收记录）。逐页并排/差分指标（MAE / changed%，越低越接近）：
  - 深色 mae（changed%）：favorites 8.62(10.79) / recent 3.08(3.55) / lookup 5.87(7.94) / sessions 3.99(3.84) / players 4.13(4.30) / danmaku 4.37(3.79) / accounts 6.65(6.90) / data 8.46(7.28)
  - 浅色 mae：favorites 15.60 / recent 2.58 / lookup 4.97 / sessions 3.17 / players 3.44 / danmaku 3.55 / accounts 5.82 / data 8.43
  - 差异来源含离线必然差异（平台头像/开播状态需真实网络、字体栅格化、选中/输入瞬时态）；composite 图与 summary 见本机验收记录。
- 已知差异：浅色 favorites 因 placeholder 对比度/页脚状态行偏高等默认态差异 MAE 偏高；收藏卡真实头像与开播 pill 需真实网络。

### 弹幕/SC 覆盖层（离线合成）

- 原版官方 demo 帧（5/12/20/40/60s + SC full/compact/badge/四角/60%/200%）与 Rust 同构场景帧（2560×1440，中灰底）：`output/overlay-freeze/`，对照清单与判据见本机验收记录。
- 已复核：SC 最新贴角顺序与 demo 一致；full/compact/scale60/200 卡宽/位置/进度条几何在 1–3px 内；同轨重叠已消失（旧帧重影为证据合成管线伪影）；快捷键提示行开关两态 PNG；resize 前后 PNG 保持轨道/进度。
- 未对齐（下一轮）：固定弹幕在 SC 存在时避让偏保守、emoji PLAIN 区段索引疑似偏移、badge/compact 细节（高度/省略号/右缘）、事件卡视觉、滚动条拖动、头像框、message_trans。

### 0.3.0-matrix（冻结树最终数字，2026-10-31）

| 矩阵 | 结果 | 说明 |
| --- | --- | --- |
| 默认 features（GUI） | **510 passed / 0 failed / 15 ignored，EXIT=0** | 26 个 test target + lib 262 + main 5 + doc-tests |
| --no-default-features | **338 passed / 0 failed / 14 ignored，EXIT=0** | overlay/danmaku/relay/core/playback 等无 GUI 覆盖 |

- ignored 均为显式性能/压力/合成 helper/#[ignore] 真实窗口 harness，非静默跳过。
- 命令：`pwsh -NoProfile -File scripts/Invoke-Cargo.ps1 -ToolchainRoot <toolchain> -TargetDirectory <ASCII target> test --locked --offline [--no-default-features] --no-fail-fast`；日志在任务 `tmp/verifier/final2/matrix-{nogui,gui}.log`。
- 冻结树修复并入：overlay paths_overlap 相对速度符号错误、overlay_layout 测试 API/断言、dm window_policy 断言、verifier ui_shot 顺序断言、overlay_layout_evidence 的 gui feature 门控。

## 0.3.0 未验证清单（只能由真实环境/用户验收）

- 真实平台取流/弹幕/登录、下播重开全周期、网络切换/休眠恢复。
- 真实播放器：mpv/PotPlayer 实际窗口、标题/日期/码率、全屏/窗口化切换、覆盖层跟随与点击穿透、SC 点击/滚轮。
- 真实 SC：付费 SC 服务端配色/头像/进度、全屏穿透、头像框。
- 真实窗口交互：chrome 拖拽/双击最大化/最小化/关闭、原生文件对话框、焦点行为、多显示器/混合 DPI。
- 11 个可配置全局热键与窗口可见性策略（模型/单测完成，overlay 接线未完成，实际仍 F7–F10 固定键）。
- 24h 稳定性与长时压测（历史 0.2.x 压测不代表本构建）。
- 其他平台真实登录、付费 AI 请求。

## 生产播放与覆盖层

- 使用实际 Rust `play` 启动器启动本机中转与 mpv，播放器实际读取 localhost FLV，查询中转计数确认媒体转发；不是将 CDN 直链交给播放器绕过中转。
- 虎牙实际视频解码、时钟推进、启动器退出后继续播放通过；真实可见的滚动弹幕通过。SC 使用合成 SC 卡片夹具；真实付费 SC 未验收。
- mpv窗口/全屏下，鼠标点击SC所在位置能到达播放器，实际暂停状态随点击改变；像素、全屏和命中穿透均通过（SC 为合成夹具，真实付费 SC 未验收）。
- 覆盖层曾发生“已渲染但不可见”和“播放器退出后残留”，已分别修复Windows layered alpha与独立进程守卫，并重测播放器/覆盖层退出。
- B站登录凭据用于实际Rust中转→mpv解码，图像尺寸/编码/字节数与时钟推进均有本机记录。
- PotPlayer真实生产启动经过Rust中转；已修复标题参数被拼入URL的引用错误。22秒复测转发3,325,004字节/3,023个tag、0请求拒绝，用户确认“视频正常，能看到弹幕”。
- PotPlayer全屏、SC命中穿透仍未验证；不能从窗口播放通过推断全屏通过。

## 登录

B站二维码曾因官方新增account域的扫码路径而被拒绝，已仅增加对应官方域与精确路径，同时保留key/协议/端口校验。用户实际扫码成功，官方健康检查明确有效；登录后B站弹幕短时接收通过。

其他平台专属浏览器及健康检测已有实现与合成回归，尚无对应真实账号验收。付费AI尚无真实密钥验收，不读取原程序或其他工具的密钥。

0.2.1 增加账号页自动复核：仅对已保存凭据的平台、串行、每平台最多每 600 秒一次，记录尝试时间，用户开始新登录时取消并作废旧结果。离线契约测试覆盖平台选择、间隔、时钟回退与检查时间文案；真实账号触发尚未单独观察。

### 登录健康语义对等（workbench 增量）

- “登录有效”超过 10 分钟：显示改为“上次有效，待复查”，原因明确“这不代表 Cookie 过期”，并保留最近检测时间与上次明确有效时间；后台自动复查仍为每平台最多 600 秒一次。
- 自动复查由启动器消息循环触发：启动后、导入/更换凭据后自动检查已保存凭据的平台；手动“检查此平台登录”同一平台最短 10 秒，冷却期内按钮禁用；“检测全部已保存平台”串行排队且同样受 10 秒下限约束。
- 凭据更换/删除：导入或清除时立即作废该平台已显示结果与在途检查；自动检查结果附带加密凭据文件指纹（MD5，仅用于比较，不显示不落盘），保存前指纹不一致或平台已无凭据则丢弃旧结果，避免旧登录状态复活。
- 账号页详情显示最近检测、上次明确有效，以及该平台房间/播放/弹幕覆盖层最近观测；超过 5 分钟的活动观测显示“待重新观测”，不表示 Cookie 过期。
- 验证边界：以上均有离线契约与纯函数回归（时钟可注入），真实账号上的 10 分钟转待复查、启动触发与后台刷新尚未单独观察。

## 本地数据与平台状态（workbench 增量）

- 新增第 9 个页面“本地数据与平台状态”（对应原版“本地数据 / 平台运行状态”）：
  - 本地数据：收藏/历史/配置/缓存/日志五类只读清单（数量、相对位置、占用、最近更新）与只读明细；配置只投影公开的播放器/弹幕/SC 选项，不显示 Cookie、AI 密钥或原始配置；缓存/日志仅列本程序运行目录下的自有文件，受保护文件名与子目录不列出、不预览；日志预览过滤凭据字段与 URL，最多显示末尾 64 KiB。
  - 维护：清理本程序运行缓存与诊断日志（仅运行目录下自有文件，二次确认；保留收藏、历史、配置、凭据与登录 profile）；分平台“退出登录”（二次确认，清除本程序该平台凭据并作废显示状态）。
  - 构建信息：Build 版本与构建来源时间（未嵌入构建戳时显示程序文件时间并明确标注）。
- 平台运行状态：按平台展示房间状态/播放解析/弹幕覆盖层三个阶段的本机最近观测、时间与房间；超过 5 分钟的活动观测标记“待重新观测”；可跳转“查看诊断日志”。该页不发起平台请求。
- 诊断日志本地有界（上限 256 KiB，超出保留尾部），写入前过滤凭据字段与 URL。
- 验证边界：纯函数、临时目录清理与写入、headless egui 帧渲染通过；真实桌面点击、资源管理器打开、退出登录后的真实播放会话表现未验收。

## 主播查询（workbench 增量）

- 查询支持“补充线索”：游戏/领域、战队/身份、外号/曾用名；仅用于对平台返回的公开分类/简介排序，平台 HTTP 仍只按昵称检索；同名账号全部保留并分别展示“线索更相符（公开资料）/身份待辨认”，绝不以线索断言本人。
- 近期搜索缓存：按平台分别缓存 10 分钟，失败或空结果不缓存；“刷新结果”绕过并撤销对应缓存；界面显示各平台是否来自缓存及原检索时间；输入/线索/平台改变使旧结果失效。
- 候选展示：头像、昵称、平台/房间/账号身份、分类/粉丝/简介摘要、账号阶段（“找到账号，房间待解析”）与核验阶段；仅抖音账号线索可进入账号→房间分阶段核验，其他平台账号线索明确提示需直播间链接或房间号。
- 别名/公会前缀匹配：全半角/大小写/空白与标点归一化；支持“公会-艺名”前缀整名匹配，拒绝粉丝/剪辑/搬运/高仿前缀；不做任意子串身份推断。
- 验证边界：合成 JSON、纯函数、缓存与排序契约通过；真实平台搜索、限流、迁移公告与真实账号核验本轮未重测（沿用此前匿名链路记录）。

## 重连与一小时稳定性

默认500现在限制连续无有效媒体进展的尝试；有效段必须满足实际媒体tag数量、字节与时间戳推进。HTTP成功、元数据或编解码头不算恢复。

- 默认500跨503次有效媒体重连，通过；无预算耗尽。
- 一小时默认500、8路并发合成断流测试，通过：各路0停滞、0解析错误；上游3,822个会话。
- 句柄216→峰值217，RSS基线18,345,984字节、峰值19,230,720字节（增长884,736字节）；合成启动器先退出，中转继续服务，播放器结束后中转回收。
- 上轮另有25分钟合成与8分钟真实流记录。一小时结果是合成媒体/断流测试，不代表一小时真实视频或24小时稳定性。

## 真实平台验收（2026-10-30，0.2.3）

无账号、无窗口、有界时间：

- B站：已保存凭据的官方健康检测 state=valid；bili:7777 真实弹幕 60s 收到 message 106、live_event 167、super_chat 1（真实付费 SC 事件），40s 收到 message 97；真实房间表情接口（已登录隔离数据目录）返回 54 个表情码（7777→真实 545068）。
- 虎牙：开播房 huya:660000 40s message 12、订阅成功 chat:1346609715；离线房 huya:333003 明确分类“房间未开播”（修复前误报 lChannelId）。
- 斗鱼：开播房 douyu:252140 / douyu:138243 30s 分别 message 12/live_event 31、message 10/live_event 134。
- 主播搜索：huya/douyu 真实搜索返回候选（头像/粉丝数/分类）；抖音搜索与流信息返回“平台要求登录或网页验证”（风控），弹幕路径已按要求停止重连并提示登录。
- 重播：huya:910323 / huya:326513 返回“房间正在重播，但平台暂未提供可用媒体，可等待直播恢复”（REPLAY-UNAVAILABLE 语义）。
- stream-info：真实 FLV onMetaData/窗口实测，斗鱼时间戳跨度 bug 已修复（douyu:252140 29.999fps/9,067ms/3280kbps；修复前 fps 0.040/span 6,088,300）。
- 发布流水线与长时真实网络证据见本机验收记录（lite/full ZIP、隐私门禁、120s×3 平台弹幕压测）。
- 未覆盖：其他三平台真实登录与登录后弹幕、真实账号 10 分钟 UI 转态、真实付费 SC 卡片渲染/点击、PotPlayer 全屏/SC 穿透、多显示器/混合 DPI、24 小时真实直播/网络切换/休眠、正式签名（均需用户或物理条件，步骤随本机验收记录给出）。

## 离线回归

0.2.4 集成后完整基线 394 通过 / 0 失败 / 13 显式忽略（测试日期 2026-10-30；本版无 commit 标识，构建标识以当次构建时间与包内 manifest 为准），以当次本机日志 notes/lead-final-regression.log 为准。默认套件仅合成数据和本机回环，不打开真实播放器、不访问平台、不读取账号；真实平台验收见 notes/real-acceptance.md。

新增覆盖：二维码/健康/域作用域、浏览器端口身份、取消与异步竞态、直播状态、连续预算、重定向策略、图片解码及缓存、过滤/延迟/合并、跨平台部分失败、AI来源/加密/过期结果、PotPlayer引用、自动登录复核调度与过期结果丢弃、事件代际顺序与凭据变更失效。

workbench 本轮新增离线用例（2026-10-30，同一独立 target 目录，合成数据/临时目录，不联网）：
```
test --test gui_contract     18 passed / 0 failed
test --test login_contract   18 passed / 0 failed
test --test settings_contract 8 passed / 0 failed
test --test platform_contract 12 passed / 0 failed
test --test sessions_contract 11 passed / 0 failed
test --test core_contract     7 passed / 0 failed
```
新增断言覆盖：登录有效 10 分钟转“上次有效，待复查”、失败保留上次明确有效时间、手动检查 10 秒下限、全部平台排队、
观测记录 5 分钟待重观测与持久化、本地数据只读清单/受保护名过滤/清理边界/日志脱敏、搜索缓存 10 分钟与刷新/
空结果不缓存、补充线索与别名/公会前缀排序、账号阶段“找到账号，房间待解析”、富媒体 OverlaySettings/行为字段
存储往返与越界拒绝。库内单元测试（lib test）158 通过 / 0 失败 / 1 显式忽略（同一 target 目录，2026-10-30），
覆盖登录事件处理（失败保留上次有效时间、凭据指纹丢弃旧结果）、搜索缓存命中/失败不缓存、会话快照观测、二维码
编码边界等；headless egui 帧渲染覆盖 9 页 × 2 种支持尺寸。工作台完成时默认套件全量复跑：334 通过 / 0 失败 / 13 显式忽略（含并行 agent 新增用例；
同一 target 目录、--locked --offline、2026-10-30；以当次本机日志为准）。

受限沙箱无法读取Win32 TCP表时，端口所有权测试明确失败；相同测试在有该API权限的环境通过。完整回归在能读取TCP表的环境执行，不跳过或放宽生产身份校验。

界面使用9页×2尺寸的真实egui绘制/网格检查（含新增本地数据页）；这不是桌面交互验收。实际播放器和扫码验证另列在上文。

## P0 对等补齐二轮（workbench，2026-10-30）

### 收藏/最近播放开播徽标

- 收藏与最近播放列表按“收藏在前、最近在后”异步查询开播状态：同时最多 4 个在途、同一房间 2 分钟内复用、
  未知/失败 20 秒后重试；选中/填入/播放及输入框旁“查询开播状态”会立即查询（单房间在途去重）。
- 徽标 [播] / [休] / [重播]；未知或查询失败不显示徽标、不冒充下播（文案“状态待确认”）。
- headless/测试构造器禁用网络查询（badge_networking=false），默认套件不会访问平台；真实平台开播状态未验收。

### 重播取流与平台文案

- B站沿用 live_status=2 且无可用播放信息 → 重播不可用；虎牙在房间页明确重播标记且无播放器配置时 → 重播不可用；
  斗鱼在取流失败/无直链时用一次官方 betard 读取 show_status/videoLoop 区分录播与下播（show_status=1 不擅自改判录播）；
  抖音只把 status=4 判下播，其他状态保持“未知”，不冒充下播，也不虚构重播态。
- 会话/失败提示按平台用“轮播 / 重播 / 录播”专有文案，不再统一写“未开播”；等待重播时保留等待重开的语义。
- 纯函数与合成 JSON 回归；真实重播房间取流未验收。

### 观看会话与播放页

- 会话行增加画质、缓冲（该会话启动时的配置）、播放器、房间状态与启动时间；“重新打开”复用该会话原来的播放器
  与缓冲配置，不随全局设置改变。
- 播放页显示最近一个非终态会话，可直接“取消启动/停止此会话”，无需跳转会话页。

### 抖音账号语法与账号入口

- 搜索框支持 https://www.douyin.com/user/<sec_uid>、裸 sec_uid、抖音号:<handle> 与 douyin:<handle>，本地识别为
  账号阶段候选（“找到账号，房间待解析”），不联网、不生成房间、不能播放或收藏。
- 账号页每平台显示最近检测/上次明确有效与房间/播放/弹幕最近观测，并提供“检测全部已保存平台”；分平台退出登录
  与缓存/日志清理入口在“本地数据与平台状态”页。网页登录仍走既有独立浏览器入口，未引入 WebView2。

二轮离线回归（2026-10-30，独立 target，合成数据/临时目录，不联网）：gui_contract 21、platform_contract 11、
sessions_contract 7、login_contract 18、settings_contract 8，均 0 失败；lib test 170 通过 / 0 失败 / 1 显式忽略
（单线程运行；默认并行时 chat_pipeline 两个时序用例曾偶发失败，单独复跑通过，非本轮文件）。默认套件全量基线
见前节。真实平台取流、开播徽标状态与重播房间均未执行真实请求。

## 抖音网页辅助（三波，workbench，2026-10-30）

- 搜索页在抖音或“全部平台”范围显示“抖音网页辅助”卡片：用户点击后，用本程序独立 Edge/Chrome 资料目录（复用
  login/browser 的 DevToolsActivePort、回环端口所有权与 WebSocket 身份校验）打开抖音官方页面；用户自行完成登录/
  验证后点击“我已登录/验证，检查并保存”，仍走既有 CDP 只读捕获（仅抖音域/路径/到期）→ 官方健康接口核验 →
  只在平台明确有效时 DPAPI 原子保存。
- 保存成功后作废搜索缓存并自动用当前关键词重查一次（仅搜索页、且当前不在查询中）；不自动选中候选、不收藏、不播放。
- 搜索失败分类：抖音验证/风控/限流错误在状态栏追加“可点击抖音网页辅助…”提示；全部平台查询中抖音受阻时保留其他
  平台结果并提示可用网页辅助。
- 退出清理：搜索页提供二次确认“退出抖音登录并清理辅助资料”，删除本程序保存的抖音凭据、作废在途健康检查、清空
  搜索缓存，并只删除数据目录下 browser-login 中名为 douyin-<32 位十六进制> 的目录；其他平台 profile、普通文件、
  异常命名与链接一律保留。清理在存储工作线程执行，失败如实显示。
- 实现差异（如实记录）：未引入 WebView2，也未像 C# 那样拦截页面内部搜索响应；捕获凭据后改走既有平台搜索接口重查，
  因此回填依赖平台接口在新会话下成功，而不是页面响应本身。
- 验证边界：离线合成回归覆盖 profile 清理作用域、网页辅助提示分类、成功/失败/非辅助登录的刷新触发、清理工作线程；
  真实抖音登录、验证码与真实网页辅助回填需用户完成，未验收。
- 三波离线回归（2026-10-30，独立 target，合成数据，无真实联网）：login_contract 19/0、gui_contract 23/0；与网页
  辅助相关的 lib 用例（决策/刷新/清理）过滤运行 5 个全部通过；同次 lib 全套 183 通过 / 1 显式忽略，另有 2 个失败
  位于并行 agent 正在修改的 danmaku_douyin/overlay 用例（非本轮文件，用例名与 panic 已报 lead）。

## 设置页路径选择与校验（三波，workbench，2026-10-30）

- 设置页 mpv / PotPlayer / yt-dlp 三个路径字段新增“浏览…”（Windows 原生 GetOpenFileNameW，raw FFI 调用
  comdlg32/user32，不新增依赖、不改 Cargo）与“自动检测”按钮；字段下方实时显示“路径有效 / 具体错误 / 留空自动查找”。
- 自动检测顺序（纯函数、注入目录，便于离线测试）：程序同级目录 → bin（mpv 另有 bin/mpv）→ PATH → 常见安装目录
  （ProgramFiles/ProgramFiles(x86) 下 mpv、DAUM\PotPlayer、yt-dlp；另含 LOCALAPPDATA 根）。不硬编码 C# 参考项目路径。
- 保存前校验（纯函数）：空值允许（走自动查找）；非绝对路径、文件不存在、不是普通文件、文件 ≤1 KiB 均拒绝保存，
  并在状态栏给出明确原因；保存失败不覆盖已保存配置。
- 原生对话框在 Windows 打开时为系统模态，会阻塞界面（预期）；取消或失败不影响界面，非 Windows 返回未选择并保留
  手工输入。对话框带 OFN_FILEMUSTEXIST/PATHMUSTEXIST/EXPLORER/DONTADDTORECENT，初始目录取当前路径的父目录。
- 验证边界：纯函数（路径校验、候选顺序、UTF-16 过滤器、Win32 结构体尺寸）离线覆盖；原生对话框的实际打开与选择需
  GUI 手工验收，本轮未自动化、未实机点击。
- 三波回归（2026-10-30，独立 target、合成临时目录、不弹真实对话框）：gui_contract 27/0、settings_contract 9/0；
  file_dialog 模块 lib 用例 6/6 通过（UTF-16 过滤器、路径校验、候选顺序、首文件检测、Win32 结构体尺寸、状态文案）。
  原生对话框的实际打开与选择仍需实机手工验收。

## 尚未验证或未迁移

- 虎牙/斗鱼/抖音真实登录、付费AI请求、真实下播到重开全周期。
- 账号页自动复核在真实账号上的触发与刷新（接口与离线调度测试已有，真实账号未单独观察）。
- PotPlayer全屏/SC穿透、全部热键实机操作、多显示器及不同DPI组合。
- 长期真实多会话、24小时真实直播、网络切换、休眠恢复。
- 本地数据/平台状态页的真实桌面交互、清理与分平台退出登录后的真实会话表现（离线函数与 headless 帧已覆盖）。
- 真实账号上的登录健康转待复查/启动触发/凭据指纹丢弃旧结果（离线契约与可注入时钟已覆盖）。
- 原版全部富媒体/SC样式、重播资源、界面设置与正式full/lite发布。

本版保持本地未提交迁移预览；最终包单独核对manifest、全部文件哈希、许可原文、凭据类文件名及可提取路径。0.2.4 预览包为 421 文件/421 manifest 条目；Test-Preview 检查 423 文件、必需条目（含 lua 脚本与 emoji 素材）13、私有路径命中 0；包内 `--version` 输出 `live-hub-rust 0.2.4`，离线 `self-test` 通过。manifest 哈希与构建记录只保留在本机本机验收记录，不写入包内文档。另：scripts/Publish-Release.ps1 可在预览包基础上产出 lite/full ZIP（portable.marker、release-manifest、逐文件 SHA256、隐私门禁；无证书时如实 unsigned），本轮已用 0.2.2 实测，0.2.3 包验收见本机验收记录。未commit/push/远端发布，不宣称已完成GitHub历史审核。

## 0.3.1 验证摘要（2026-10-31，第二轮）

本轮为 0.3.0 用户实测缺陷修复与遗留收口。默认离线回归、合成证据与真实网络单项验证如下；真实桌面交互仍需用户验收。

### 已修复项与证据

- **下拉框交互**：headless 帧测试（打开→下一帧仍开→点行选中关闭→重开→外部点击关闭→外部点击不改值）通过；launch 卡与设置页共用 helper。
- **头像解析**：真实网络 ignored 测试 bili:7777 → i0.hdslb.com 96px PNG 11716B → 服务端 48×48 PNG 3423B + index.json 落盘；huya/douyu 可下载；douyin 风控时分类为 Blocked。日志 notes（本机验收目录 tmp/r2/avatars-real-network.log）。
- **弹幕右缘/顶部裁切**：根因 scheduler viewport 未同步（构造 640×240）；fixture overlay-right-edge（1280×720，freeze 4s/6s）显示消息完整从右缘 x≈1070..1240 进入并 2s 左移≈380px；旧 0.3.0 对照帧在本机验收记录。
- **覆盖层遗留**：窗口策略合成真窗证据（最小化隐藏/还原显示/隐藏播放器隐藏、topmost 开关）；SC badge/compact/scale200/face_frame/message_trans、事件卡、合并 ×N、fixed 避让、规则接入均有 output/r2/ 合成帧；lib overlay 44/44、hotkeys 6/6、window_policy 6/6、rules 7/7、diag 5/5、sc_fields 3/3、layout evidence 3/3。
- **收藏导入导出**：favorites_transfer 契约 7/7（导出→Store::import_json 往返、字节稳定、边界拒绝）；原生保存对话框结构尺寸断言。
- **界面字体/热键**：gui::theme 19 passed、workbench::danmaku 3 passed、widget_gallery 2 passed；ui-shot 证据 output/r2-ui-shots/。
- **UI 还原回归**：R2 8 页截图 output/rust-ui2-dark/、选中态/窄窗/tab5 证据；与 0.3.0 参考对比指标沿用 §0.3.0。

### 0.3.1-matrix（冻结树，2026-10-31）

| 矩阵 | 结果 | 说明 |
| --- | --- | --- |
| 默认 features（GUI） | **587 passed / 0 failed / 17 ignored，EXIT=0** | 冻结树，hash 前后一致（verifier 2026-10-31 勘误：早前汇总脚本误报 541/0/12，同日志重算为 587/0/17） |
| --no-default-features | **393 passed / 0 failed / 16 ignored，EXIT=0** | 冻结树，hash 前后一致 |

- 定向验收：avatars_real_network ignored 2/2（bili:7777 取到 hdslb 头像并缓存 48×48 PNG；huya/douyu 可下载；douyin 风控 Blocked）；favorites_transfer 7/7 + e2e；dropdown 交互测试在 GUI 矩阵内通过；ui_font_settings_acceptance 全绿（130% A/B 截图比例≈1.27–1.28）；rules/diag/sc_fields/lookup_fields/relay_log/hotkeys/window_policy/URL 测试均在两矩阵内绿。
- 证据：notes/evidence-r2/matrix-gui-final.log、matrix-nogui-final.log、avatar-network-final.log；树一致性 notes/r2-final-before.txt / r2-final-after.txt（121 行哈希一致）。

### 0.3.1 UI 截图回归（最终树）

- 8/8 页 2480×1440，不抢前台（采样 133 次，窗口从未成为前台）；MAE（深色）：favorites 8.90 / recent 3.14 / lookup 5.93 / sessions 4.05 / players 4.20 / danmaku 4.42 / accounts 6.72 / data 8.65；产物 output/r2-capture/final/、output/r2-compare/final/。
- 覆盖层 10/10 合成场景渲染成功（output/overlay-r2/final/）；badge 高 57、右缘 2539；compact 3 卡 h72/右缘 2539；full 252+progress8/右缘 2540；scale60 151/105；scale200 504/352（背景黑底合成部分不可测）。

### 0.3.1 未验证清单

- 真实桌面：全局热键真实按键、鼠标拖动滚动条、下拉真实点击、原生导入/导出模态对话框、多显示器/混合 DPI、独占全屏。
- 真实环境：付费 SC、真实平台登录、真实 mpv/PotPlayer 标题观感、24h 稳定性。
- 彩色 emoji 精灵图 PLAIN 区段偏移（原版资产生成链问题，下一轮）。
- 浅色主题个别默认态对比度。

## 0.3.2 验证摘要（登录浏览器隐私修复）

- 变更范围：src/login/browser.rs（登录浏览器偏好 Chrome、独立临时资料、关闭同步/Edge 隐式登录特性）、两处账号页/登录进度文案、版本号；不涉及播放/弹幕/UI 页面逻辑。
- 验证：cargo check --locked --offline --all-targets EXIT=0；cargo test --lib **296 passed / 0 failed / 2 ignored**（含 login_browser_never_signs_into_the_user_account）；Chrome 真实启动探测：独立资料下 DevToolsActivePort 正常、Chrome/156.0.8078.80、无个人账号资料。
- 0.3.2 两矩阵（冻结树）：默认 GUI **588 passed / 0 failed / 17 ignored**；--no-default-features **394 passed / 0 failed / 16 ignored**，EXIT=0。0.3.1 的 UI 对比/覆盖层证据仍适用（本轮仅登录浏览器启动参数与文案变化）。
- 未验证：真实虎牙/斗鱼/抖音扫码与登录后 Cookie 捕获、真实 Edge-only 机器上的同步提示（已关闭相关特性并写文案，以实机观察为准）。

## 0.3.3 验证摘要（抖音网页登录 Cookie 容错修复）

- 变更范围：src/login/browser.rs（Cookie 逐条容错/诊断/新单测）、账号页说明文字合并、版本号；不涉及播放/弹幕/UI 页面逻辑。
- 验证：cargo check --locked --offline --all-targets EXIT=0；cargo test --lib **298 passed / 0 failed / 2 ignored**（新增 decode_skips_malformed_cookies_and_keeps_the_session、decode_reports_when_every_cookie_is_unusable）。
- 真实现场：用户登录窗口 Cookie 集合 68 个，66 个名称合法、1 个空名称（www.douyin.com）——与“单条异常 Cookie 导致整批失败”根因一致；修复后该条被跳过。
- 最终两矩阵（冻结树，2026-10-31）：默认 GUI **590 passed / 0 failed / 17 ignored**；--no-default-features **396 passed / 0 failed / 16 ignored**，EXIT=0（0.3.2 基线 588/394 各 +2 个新单测，无回归）。
- 未验证：真实抖音扫码登录保存后的取流/弹幕需用户实机复核。

## 0.3.4 验证摘要（抖音主播搜索恢复）

- 变更范围：src/platforms.rs（抖音搜索完整查询参数/浏览器请求头、2 个离线测试、env 门控诊断）、版本号/文档；不涉及播放/弹幕/UI 页面逻辑。
- 真实验证：使用用户凭据库副本（不打印 Cookie）调用 search_authenticated(Douyin, "花小龙") → **10 个候选**（status_code=0, user_list=10，首个为花小龙主页）；修复前同一调用为 0。
- 离线：cargo test --lib **300 passed / 0 failed / 2 ignored**（新增 douyin_search_url_matches_the_official_web_request_shape、douyin_search_result_maps_profile_identity）。
- 最终两矩阵（冻结树，2026-10-31）：默认 GUI **592 passed / 0 failed / 18 ignored**；--no-default-features **398 passed / 0 failed / 17 ignored**，EXIT=0（0.3.3 基线 590/396 各 +2 新单测、+1 ignored 手工探针，无回归）。
- 未验证：真实界面检索/候选展示/核验/收藏需用户实机复核。

## 0.3.5 验证摘要（Edge 隐私登录 + 抖音搜索收敛）

- 变更范围：src/login/browser.rs（优先 Edge + InPrivate、参数/测试）、src/gui.rs（搜索缓存只存非空、空/失败重试一次、搜索诊断日志、登录文案）、账号页文案、版本号；播放/弹幕逻辑不变。
- 离线：cargo check --all-targets EXIT=0；cargo test --lib **300 passed / 0 failed / 2 ignored**。
- 真实：Douyin 搜索（本机凭据库、不打印 Cookie）返回 10 个候选；本机 Edge 与 Chrome 均安装，生产会选择 Edge+InPrivate（真实 InPrivate 扫码登录需用户验收）。
- 数据：原版 C# 收藏已导入本机 Rust 数据目录（34 条，33 新增+1 更新）。
- 最终两矩阵（0.3.5 冻结树）：默认 GUI **592 passed / 0 failed / 19 ignored**；--no-default-features **398 passed / 0 failed / 18 ignored**，EXIT=0（与 0.3.4 相比 passed 不变、ignored 各 +1 为新增手工抖音双平台探针；同一冻结树哈希前后一致）。

## 0.3.6 验证摘要（查找页 7 项修复）

- 变更范围：查找页候选行/AI 面板/排序/选中自动核验、DeepSeek 设置收起、收藏选中描边、候选头像队列、抖音公开核验请求（platforms.rs public_json_get）；播放/弹幕逻辑不变。
- 离线：cargo check --all-targets EXIT=0；cargo test --lib **301 passed / 0 failed / 2 ignored**；lookup_fields_contract **6 passed**；avatars_contract **13 passed**。
- 真实：Devin 登录态搜索后逐候选 verify 不再报 outside selected platform；当前无人在播，reflow 路径由单测/公开请求测试覆盖。
- 离屏截图：notes/evidence-r7/（候选布局、AI 面板、收藏描边）。
- 最终两矩阵（0.3.6 冻结树）：默认 GUI **594 passed / 0 failed / 20 ignored**；--no-default-features **400 passed / 0 failed / 19 ignored**，EXIT=0；同一冻结树哈希前后一致（122 行）。相对 0.3.5 为 +2 passed、+1 ignored（新增手工探针），无回归。

## 0.3.7 验证摘要（文字堆叠/快捷键重试/离线 web_rid/头像索引）

- 变更范围：lookup 空白归一化+详情单段；hotkeys 队列预建+失败重试；platforms.rs 抖音离线 web_rid 兜底与 Huya 别名头像；avatars.rs remembered URL 持久化 + verify 接线；C# 34 条头像索引本机迁移。
- 离线：cargo check --all-targets EXIT=0；cargo test --lib **305 passed / 0 failed / 2 ignored**；新增单测：抖音 web_rid/roomId 解析、Huya 别名 profileRoom、remembered URL 优先与持久化。
- 真实：抖音逐候选 verify 不再跨域报错（离线无公开 web_rid 时明确提示）；Huya a16789/clkimmy 头像 URL 解析成功；ui-shot 收藏 B站真实头像。
- 最终两矩阵（0.3.7 冻结树）：默认 GUI **598 passed / 0 failed / 22 ignored**；--no-default-features **404 passed / 0 failed / 21 ignored**，EXIT=0；同一冻结树哈希前后一致（122 行）。相对 0.3.6 为 +4 passed、+2 ignored，无回归。

## 0.3.8 验证摘要（抖音解析路径 / 收藏回填 / 无乱码 / 直播优先）

- 变更范围：platforms.rs 抖音抖音号→直播页→数字房间仓库兜底；lookup 收藏名称自动回填与 emoji 显示清洗；search 可能性标签；favorites 按直播状态排序。
- 离线：cargo check --all-targets EXIT=0；cargo test --lib **307 passed / 0 failed / 2 ignored**。
- 真实：抖音 coke 搜索后逐候选 verify，coke老师 等返回 989125753255（offline，可收藏）；跨域 Cookie 不再报错。
- 最终两矩阵（0.3.8 冻结树）：默认 GUI **600 passed / 0 failed / 22 ignored**；--no-default-features **404 passed / 0 failed / 21 ignored**，EXIT=0；同一冻结树哈希前后一致（122 行）。相对 0.3.7 GUI +2 passed，无回归。

## 0.3.9 验证摘要（MPV autosync / 斗鱼公开取流 / relay 日志）

- 变更范围：playback.rs mpv --autosync=30；platforms.rs 斗鱼取流 API 改 public 无 Cookie 请求；relay 子进程继承 LIVE_HUB_DATA_DIR 写真实数据目录日志。
- 离线：cargo check --all-targets EXIT=0；cargo test --lib 307 passed / 0 failed / 2 ignored。
- 真实：douyu:633019 带登录态 resolve 成功；抖音 926530112088/768392702185 + PotPlayer 本机可正常播放。
- 最终两矩阵（0.3.9 冻结树）：默认 GUI **600 passed / 0 failed / 24 ignored**；--no-default-features **404 passed / 0 failed / 23 ignored**，EXIT=0；同一冻结树哈希前后一致（122 行）。相对 0.3.8 passed 不变、ignored 各 +2（新增手工探针），无回归。

## 0.3.10 验证摘要（PotPlayer 标题参数 / 抖音 jomodns 跳转 / MPV autosync）

- 变更范围：playback.rs PotPlayer 标题探测（走中转连接读 onMetaData）；platforms.rs 媒体白名单增加 jomodns.com；0.3.9 的 mpv --autosync=30 与斗鱼公开取流保留。
- 离线：cargo check --all-targets EXIT=0；cargo test --lib 309 passed / 0 failed / 2 ignored。
- 真实：抖音 796794251979 经 relay+PotPlayer 正常播放，标题后缀 1920x1080 62fps h264 5880kbps；mpv 可播。
- 最终两矩阵（0.3.10 冻结树）：默认 GUI **602 passed / 0 failed / 24 ignored**；--no-default-features **406 passed / 0 failed / 23 ignored**，EXIT=0；同一冻结树哈希前后一致（122 行）。相对 0.3.9 各 +2 passed，无回归。

## 0.3.11 验证摘要（四平台标题统一/探测兜底）

- 变更范围：Douyin 基础标题补 owner、Huya 别名房标题解析、PotPlayer 标题探测 relay→direct 兜底；0.3.10 的 jomodns 白名单/PotPlayer 标题探测保留。
- 离线：cargo check --all-targets EXIT=0；cargo test --lib 309 passed / 0 failed / 2 ignored。
- 真实：PotPlayer 窗口标题已实测 B站/斗鱼/抖音为“主播 - 标题 · 参数”；虎牙别名基础标题已恢复（同探测路径）。
- 最终两矩阵（0.3.11 冻结树）：默认 GUI **602 passed / 0 failed / 24 ignored**；--no-default-features **406 passed / 0 failed / 23 ignored**，EXIT=0；树哈希 before/after 完全一致（122 行）。与 0.3.10 数字一致（本轮无新增测试），相对 0.3.9 各 +2。

## 0.3.12 验证摘要（B站 smtcdns 302 放行）

- 变更范围：platforms.rs 媒体 CDN 白名单增加 smtcdns.net（B站直播 302）；0.3.11 标题统一修复保留。
- 离线：cargo check --all-targets EXIT=0；cargo test --lib 310 passed / 0 failed / 2 ignored。
- 真实：B站 7777 PotPlayer 标题 老实憨厚的唐 - 德云色 热游畅玩...  ·  1920x1080 60fps h264 8836kbps。
- 最终两矩阵（0.3.12 冻结树）：默认 GUI **603 passed / 0 failed / 24 ignored**；--no-default-features **407 passed / 0 failed / 23 ignored**，EXIT=0；树哈希 before/after 完全一致（122 行）。相对 0.3.11 各 +1 passed。


## 0.3.12 额外验证（F7-F10 固定键）

- 固定键改为 GetAsyncKeyState 轮询（播放器前台时触发），不再依赖 RegisterHotKey；编译与 lib 测试 310/0/2。
## 0.3.15 验证摘要（收藏页右侧会话列表）

- 变更范围：`src/gui/workbench/favorites.rs`（会话列表顺序改为最新在前、按 Platform:Room 去重、每行向 ScrollArea 申报高度使滚动条/滚轮生效、上限 24 行）。
- 离线：`cargo check --all-targets` EXIT=0；默认 GUI 矩阵 **615 passed / 0 failed / 26 ignored**（42 target，EXIT=0）；`cargo test --lib gui` 115 passed / 0 failed；`cargo test --test gui_contract` 27 passed / 0 failed。
- 预览包：`artifacts/preview-0.3.15`（439 文件 / exe 21,201,741 B / sha256 `e0a78698a56b1ffa8b17d2f9c96e092c46d5d40f995907d22baa49b0c21aeb3f`）；Test-Preview：FilesChecked 441 / Entries 439 / ManifestMatches true / PrivatePathFindings 0 / EXIT=0；包内 `--version` = 0.3.15，`self-test` 7/7；`artifacts/current` 已切到本版，preview-0.3.14 保留回退。
- 离屏证据：隔离数据目录 + 12 条合成历史的 ui-shot（`output/uishot-sidebar/favorites.png`）显示右侧列表出现滚动条且仅绘制可视行；会话为内存态，正在播放行的排序只能真实窗口验收。

## 0.3.14 验证摘要（虎牙短号房间开播状态）

- 变更范围：`src/room_state.rs` 虎牙分支增加「页面数字房号 → 四态接口」回退；新增页面房号解析纯函数与单测、真实接口人工测试（`#[ignore]`）。
- 离线：`cargo check --all-targets` EXIT=0；默认 GUI 矩阵 **615 passed / 0 failed / 26 ignored**（42 target，EXIT=0；相对 0.3.13 +1 passed，新增房间号解析单测）；`cargo test --lib room_state` 3 passed / 1 ignored。
- 预览包：`artifacts/preview-0.3.14`（438 文件 / exe 21,200,610 B / sha256 `474479b9df3bd36e33b965fe5b73291fa74fed279fceef3d9bf2959065a92683`）；Test-Preview：FilesChecked 440 / Entries 438 / ManifestMatches true / SortedManifest true / PrivatePathFindings 0 / EXIT=0；包内 `--version` = 0.3.14，`self-test` 7/7；`artifacts/current` 已切到本版，preview-0.3.13 保留回退。
- 真实接口（2026-11-01 本机）：`a16789`（安德罗妮）profileRoom 短号查询 422 → 页面 `"profileRoom":528300` → 数字房号查询 `liveStatus=ON` → 「直播中」；修复前同一房间为 Unknown。
- 未验证/限制：虎牙页面若改版去掉数字房号字段，状态会如实回到「状态待确认」（不误报）；其余平台状态逻辑未改。
## 0.3.13 验证摘要（覆盖层穿透 / 固定键吞键 / SC 滚轮 / 主界面圆角）

- 变更范围：
  - `src/overlay.rs`：主覆盖层窗口永久 `WS_EX_TRANSPARENT`；新增 alpha=0 分层输入窗口（`LiveHubRustScInput`，`SetWindowRgn` 裁到 SC 卡片/关闭按钮/滚动条矩形）承接点击与滚轮；卡片上移动时向播放器转发 `WM_MOUSEMOVE`；SC 命中矩形裁剪到可见带；`SC_WHEEL_STEP_POINTS = 72`（旧版 raw delta*0.6 等价）；固定 F7–F10 改为播放器前台时 `RegisterHotKey` 注册、离开注销、失败键 `GetAsyncKeyState` 兜底。
  - `src/hotkeys.rs`：`set_fixed_active` / `poll_events`（`HotkeyEvent::{Action,Fixed}`）/ `fixed_registered` / `fixed_errors`，固定键 id 基址 `0x4D00`。
  - `src/gui/window_frame.rs`（新增）+ `src/gui.rs`：Windows 11 `DwmSetWindowAttribute(DWMWA_WINDOW_CORNER_PREFERENCE=33, DWMWCP_ROUND)`，动态加载 dwmapi（GNU 工具链无 import lib），`EnumThreadWindows` 精确标题取本进程 UI 线程的 HWND，失败全部 no-op + 诊断；非 Windows/Win10 保持直角。
- 离线：`cargo check --all-targets` EXIT=0；GUI 矩阵 **614 passed / 0 failed / 25 ignored**（42 个 target，EXIT=0）；`--no-default-features` 矩阵 **416 passed / 0 failed / 24 ignored**（42 个 target，EXIT=0）。两矩阵均在 0.3.13 冻结树上跑完。
- 合成运行证据（离线 fixture，不接触真实平台）：`LiveHub Rust Overlay` 主窗 `ExStyle=0x80801B0`（含 `WS_EX_TRANSPARENT`，永不参与命中）；`LiveHubRustScInput` 输入窗 `ExStyle=0x8080080`（`WS_EX_LAYERED|TOOLWINDOW|NOACTIVATE`，无 TRANSPARENT，alpha=0），窗口区域 bbox 只覆盖右上 SC 卡片区，20×20 `PtInRegion` 命中图只有卡片区命中——区域外像素由系统直接跳过输入窗，跨进程落到播放器。证据见本机验收目录 `output/smoke-sc-input.txt`。
- 预览包：`artifacts/preview-0.3.13`（437 文件 / exe 21,195,821 B / sha256 `1c48f058d6e0a9f5457864037ab5599e19bc5cf42d013fc8a4bbfa48c78924c0`）；Test-Preview：**FilesChecked 439 / Entries 437 / ManifestMatches true / SortedManifest true / RequiredEntries 13 / PrivatePathFindings 0 / EXIT=0**；包内 `live-hub-rust.exe --version` = 0.3.13，`self-test` **7 passed / 0 failed**。`artifacts/current` 已从 preview-0.3.12 切到 preview-0.3.13，preview-0.3.12 保留回退。
- 真实/桌面未验证（用户验收项）：真实弹幕平台下的 SC 点击/滚轮/滚动条拖动、PotPlayer 全屏底部控制栏、F7/F8/F9/F10 是否只触发覆盖层（播放器不再响应）、主界面 DWM 圆角观感、多显示器/混合 DPI。0.3.13 的固定键注册依赖「播放器前台」判定，全屏独占模式下的前台判定仍属用户验收项。

## 1.0.1 验证摘要（抖音弹幕图片表情字典与 WebP 渲染）

- 变更范围：`src/danmaku_douyin.rs` 新增 `EMOJI_LIST_URL` 与 `fetch_emoticons_result`（白名单校验 → 浏览器 UA/系统代理客户端 → 可选 douyin 域 Cookie → Referer/Accept 头 → HTTP 状态 → 1 MiB 长度与 `take` 上限 → UTF-8 → `parse_douyin_emoticons` → 非空校验）；`src/danmaku.rs` 新增四个 fetch 包装函数、抖音连接前后台 `douyin-emoticons` 线程与 `EMOTICON_CACHE_VERSION`/`cached_emoticons_version()`；`src/danmaku_rich.rs` 表情解析遍历全部 `url_list` 镜像并跳过空代码；`src/overlay.rs` 按缓存代次刷新字典、`LIVE_HUB_EMOTICON_JSON` 兼容抖音 `emoji_list` 形状；版本、能力清单、对照清单与发布说明同步 1.0.1。
- 根因：不是 WebP 解码缺陷。Rust 版此前只接了 B站 房间表情字典，抖音 `emoji/list` 解析函数虽已具备但没有生产调用，覆盖层字典为空，`[捂脸]` 一类代码全部按原始文本绘制。
- 离线矩阵（44 个 test target，干净环境，`--locked --offline`）：默认 GUI **626 passed / 0 failed / 27 ignored，EXIT=0**；`--no-default-features` **422 passed / 0 failed / 26 ignored，EXIT=0**。新增：lib 单测 4（多镜像 URL/空代码、缓存代次、覆盖层刷新、抖音本地字典），集成验收测试 2（env 门控，默认套件离线跳过 1 ignored / pass）。日志：本机验收记录、`notes/review-matrix-nogui.log`。
- 既有缺陷（非 1.0.1 引入，本任务修复）：1.0.0 已把 `docs/capabilities.json` 的 `stage` 改为 `release`，但 `tests/cli_contract.rs` 仍断言 `migration-preview`，只在跑全量矩阵时暴露；现将断言改为 `release`。首次失败证据：`notes/review-matrix-gui-firstrun.log`。
- 真实匿名接口（2026-11-01 本机，无 Cookie、无登录、无房间号）：`GET https://www.douyin.com/aweme/v1/web/emoji/list/` 返回 HTTP 200，`emoji_list` 214 条（约 118 KB），含 `[捂脸]`/`[微笑]`；428 个镜像 URL 全部属于 `douyinpic.com` 家族并通过 `images::approved_url`。
- 真实解码与链路：4 张本地真实 VP8L 样本（96×96）经生产 `images::decode` 全部成功（RGBA 36864 字节、非全零，`<html>` 返回 Err）；生产 `ImageLoader` 真实下载 `[捂脸]` 图并解码为 96×96（像素统计与本地样本一致）；`split_text_segments("[捂脸]", dict, None)` 恰好得到 1 个 `Emoticon` 段。证据：本机验收记录、`notes/verify-fetch-e2e.md`。
- 离线与真实直播间渲染证据：旧 1.0.0 无字典时把 `[捂脸]` 画成字面文字（复现报障）；同一旧构建注入真实 URL 字典后即显示表情图片（说明解码/渲染链路本来可用）；新 1.0.1 读取抖音原始 `emoji_list` 形状同样显示图片；新 1.0.1 在真实直播间 3010151431 匿名连接后状态为“抖音: 已收到服务端推送，弹幕开始”，弹幕表情以图片绘制。截图与哈希：`output/overlay-render/`、`output/overlay-real-douyin-3010151431/`、`output/render-evidence.json`（本机验收目录）。
- 未验证/限制：平台自定义/付费表情包与需要登录权限的表情；长时间/多房间/断线重连稳定性；不同代理环境（该端点走系统代理，本机直连环境未见异常但不作为保证）；离线渲染证据不是真实播放器窗口（真实直播间覆盖层截图已单独采集）。
- 程序包与入口：`artifacts/release-1.0.1`（441 文件 / exe 21,222,639 B / sha256 `4c248e86b226e92cb4a7d6aa8e7950f2c5c9ff9f238504693e6f17ac977823f5`）；`Test-Preview`：FilesChecked 443 / Entries 441 / ManifestMatches true / SortedManifest true / RequiredEntries 13 / PrivatePathFindings 0，EXIT=0；包内 exe `--version` = 1.0.1，`self-test` 7/7，`capabilities` 含 `douyin-emoticon-dictionary-and-webp-rendering`；`artifacts/current` junction 已从 release-1.0.0 切到 release-1.0.1，开始菜单「看直播（Rust）」目标/参数/图标未变，release-1.0.0 保留回退。

## 1.0.2 验证摘要（PotPlayer 播放中反复「缓冲」排查与中转稳健性修复）

- 变更范围：`src/flv.rs`（`ReconnectTimeline` 增加同一响应内大时间戳跳变重基、可带符号时钟修正、`jumps()/last_jump_ms()` 诊断；新增 2 个单测并更新时钟重启用例）、`src/relay.rs`（播放器写超时 5 s→15 s；正常媒体分段结束后的重连等待 `min(retry_delay,150 ms)`；`timeline_jumps` 聚合/连接计数与 WARN 日志）、版本与文档；播放/弹幕/UI 逻辑不变。
- 根因与证据（2026-11-01 20:36–21:06，真实虎牙房间，全程只读、不打断用户播放）：用户播放连接 #3 稳态 103.2 tag/s、媒体/墙钟 1.0002x、无重连；20:44:58–20:50:25 上游间歇慢段（用户连接平均 0.77x、最低 21 tag/s；同期独立只读 tap 无播放器反压也只有 0.22–0.37x）→ 播放器 2 s 级读缓存欠载出现「缓冲」；慢段结束上游在同一条响应内出现 +114,517 ms 音视频同步跳变（前后 tag 到达间隔 0.52 s、后接 112 KB 关键帧、无重连/无 segment ended）。直连 CDN 复核仍是分段响应（0.28–0.41 s 吐 0.93–3.40 MB 后 EOF、会话间内容重叠、同一 URL 约 40 s 用废）。
- 离线验证：`cargo check --locked --offline`（默认 GUI）通过；`cargo test --lib flv::tests` **14 passed / 0 failed**（含新增 `rebases_a_mid_stream_forward_discontinuity_without_a_reconnect`、`rebases_a_mid_stream_backward_discontinuity`）；`cargo test --no-default-features --test relay_contract --test reconnect_policy --test relay_log_contract --test playback_contract` **31 passed / 0 failed / 4 ignored，EXIT=0**（relay_contract 14+4 ignored、reconnect_policy 8、relay_log_contract 3、playback_contract 6）。
- 运行态对照：健康时段两只独立 tap 合计 440 s，>1 s 到达空档 0 次（max 0.909 s），稳态媒体/墙钟 1.0002x、视频 60.0 tag/s；播放器模型（读缓存 2 s，PotPlayer 形状；达到上限即暂停读取）与 15 s 模型各 480 s：**0 次欠载**、媒体速率 1.008x/1.016x。
- 程序包与入口：`artifacts/release-1.0.2`（442 文件 / exe 21,226,876 B / sha256 `5401EF6532F1AFB3275A78A00685184E76E61E4557B1F915FCBC6614B2826B16`）；Test-Preview：FilesChecked 444 / Entries 442 / ManifestMatches true / SortedManifest true / RequiredEntries 13 / PrivatePathFindings 0，EXIT=0；包内 exe `--version` = 1.0.2、`self-test` 7/7；`artifacts/current` junction 已从 release-1.0.1 切到 release-1.0.2（release-1.0.1 保留回退）；包内 442 文件与 manifest 一致。
- 未验证/限制：本版未在用户真实 PotPlayer 会话上做 A/B（用户正在观看，未重启）；`timeline_jumps` 需下次慢段复现时观察；上游持续慢于实时（本次约 5 分钟 0.22–0.37x）时本地无法避免欠载，建议同时排查 Clash TUN/代理节点质量；PotPlayer 26.10.21 无公开可编程的网络缓冲/读取超时设置（本机注册表全量与 DLL 字符串检索确认），程序不擅自改用户播放器配置。

## 1.0.3 虎牙、斗鱼弹幕表情（2026-11-02）

- 根因：虎牙、斗鱼默认表情字典漏迁移，弹幕文本中的方括号代码没有对应图片 URL；虎牙“满头问号”的精确 CDN 主机也缺少图片下载许可。
- 修复：连接前加载虎牙 40 个不同代码、斗鱼 250 个默认代码；原版已知虎牙覆盖项优先；初始图片请求及所有重定向使用相同主机校验。
- 默认完整测试：632 passed / 0 failed / 29 ignored（GUI 功能启用，依赖锁定且离线运行）。包括新增虎牙 V1/V2 解码后连续表情/中文混排/未知文字回退、全部映射分段与 CDN 边界测试。起初受限沙箱中的两个既有测试因本机端口/系统查询限制失败，随后在可访问本机回环端口的环境完整重跑通过。
- 显式联网验收：虎牙 40/40、斗鱼 250/250 个真实图片均通过生产 ImageLoader 下载、图片容器/像素尺寸限制与 RGBA 解码检查；未使用 Cookie、账号、浏览器或播放器。
- 边界：没有对正在播放的真实窗口做强制重启或视觉验收；默认/已知补充字典不覆盖全部服务端、付费及房间专属包。未知代码、图片下载中或失败、关闭图片显示时保留文字。

## 1.0.4 验证摘要（2026-11-06）

- 变更范围：`src/relay.rs`（结构化重定向诊断/遥测/策略单测）、`src/platforms.rs`（CDN 家族、抖音房间页闸门/重试/冷却、虎牙候选探测、搜索分页、抖音直链核验）、`src/flv.rs`（代际时间线边界与守卫单测）、`src/playback.rs`（RelayTelemetry 诊断字段）、`src/sessions.rs`（显式 resync）、`src/search.rs` / `src/gui*` / 弹幕模块（工作台、规则、恢复、尺寸与解码）；版本与文档。
- 构建与测试（本机，隔离工具链、`CARGO_NET_OFFLINE=true`）：`cargo build --release` 通过；`cargo test --locked --offline` 全部目标通过（含新增 flv 代际/两小时跨度/u32 上限、重定向策略脱敏、规则契约、队列优先级、搜索分页等合成回归；默认测试不联网）。
- 程序包与入口：`artifacts/release-1.0.4`（462 文件 / exe 23,353,085 B / sha256 `31d13f8a17a0aa69b03696e34fde4cc35f12399e590ac56cd8419ba46a02fa47`）；Test-Preview：FilesChecked 464 / Entries 462 / ManifestMatches true / SortedManifest true / RequiredEntries 13 / PrivatePathFindings 0；包内 exe `--version` = 1.0.4、`self-test` 7/7；`licenses/rust/SOURCES.json` 219 包、missingOriginals 0（alloc-stdlib 0.2.4 用上游 LICENSE 快照补齐，SHA-256 校验通过）。
- 未验证/限制：真实抖音房间（登录/验证码/风控）取流与弹幕、真实长时音画同步、物理声画输出测量、多显示器与休眠/网络切换；验收协议见任务 `2026-11-06-看直播双版本修复/output/音画同步验收协议.md`。

