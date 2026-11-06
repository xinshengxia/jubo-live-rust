# live-hub-rust

「看直播」的独立 Rust 重构：B站、虎牙、斗鱼、抖音取流，mpv / PotPlayer 播放，本机 FLV 中转，透明弹幕与 SC 覆盖层，登录、收藏、历史、观看会话与主播搜索。

当前版本为 **1.0.6**（抖音起播恢复链：业务失败换路、系统代理回退、验证刷新后单次重试、CDN/协议兼容、同房间请求合并；抖音礼物/进场/点赞富事件）。使用独立数据目录，保留原程序与入口；不会读取原版 Cookie、API Key、浏览器资料、收藏或历史。实测范围见 [验证报告](docs/validation.md)，功能差异见 [对照清单](docs/parity-gap.md)。


## 本版变化（1.0.6）

- 抖音房间页改为业务结果驱动换路（pinned IPv4 → 系统解析 → 只读 Windows 系统代理，TUN fake-IP 保留）；200 验证码/空壳/格式变化页不再提前结束。
- 验证恢复：专属 profile 隐藏浏览器静默刷新一次（单飞 + 冷却/退避 + 跨进程锁）后只重试当前房间一次；账号页新增「抖音网页验证」；中继进入 verification 等待态不耗重连预算。
- 媒体地址不再要求固定 CDN 家族，改为公网 DNS 策略（逐跳钉扎保留）；保留 http 协议、拒绝 https→http 降级、跨主机不转发 Cookie/Origin、Referer 受控。
- 状态/标题/头像同房间单飞 + 4 秒短复用（凭据变化即失效），起播始终新取；FLV 画质键大小写不敏感；验证码判定收窄。
- 抖音富事件：Gift/Member/Like 映射到既有 Live 契约（协议来源已核实、未实机验证）。
- 详细：[1.0.6 发布说明](docs/release-notes-1.0.6.md)。

### 1.0.5 修复（历史）

- 修复跨平台翻页的失败页被跳过，以及切换候选时查询或 AI 等待状态无法结束。
- 满队列优先让普通聊天让位于 SC 与删除事件；修复固定快捷键注册失败后的重试与失焦事件过滤。
- B站 UID 可核验到直播间；取流工具回退保留本程序登录凭据。重新同步会话避免迟到事件覆盖停止状态。
- 观看会话显示中继时钟诊断，并可记录音画异常时刻。
- 详细：[1.0.5 发布说明](docs/release-notes-1.0.5.md)。

### 1.0.4 修复（历史）

- 抖音媒体重定向改为结构化拒绝诊断（规则/阶段/脱敏主机/状态码进日志与遥测），补齐常见 CDN 家族并支持 http→https；房间页 IPv4 优先、系统解析回退、空壳页重试与 6/12/18 秒冷却，保存 Cookie 时补新 nonce。
- 虎牙取流保留全部候选线路（hs.* 优先）并逐条安全探测（≤4 条 / 10 秒预算），失败记录原因后切换。
- 中继时间线改为代际时钟：旧音频尾部不再继承新偏移，两轨确认新代际后继续；跨断点未确认代际 5 秒守卫防止饿死；32 位输出时钟用尽时结束并提示重开会话。`RelayTelemetry` 新增 timeline_*/av_delta_*/policy_rejections/last_policy_reject 诊断。
- 弹幕断线恢复改为连续失败预算，稳定接收后复位；工作台可编辑用户屏蔽（uid:/name:/re:/包含）与高亮/置顶；合并去重不再断高亮 ID；`re:` 改用 regex crate 线性引擎；GIF/BMP 静态表情与 Small/Large 尺寸选择生效。
- 搜索工作台：分页、粘贴链接/分享/uid 分流、来源摘录可滚动、切换查询清旧选择、收藏核验门槛、AI 新候选闭环、会话页停止/重新同步与中继诊断、数据页覆盖生产日志、收藏删除确认/撤销、外链安全打开。
- 详细：[1.0.4 发布说明](docs/release-notes-1.0.4.md)。

### 1.0.3 修复（历史）

- 虎牙连接前加载表情字典，修复 `[666]`、`[笑哭]` 等显示为方括号文字的问题。迁移原版 39 个默认表情及 5 条补充/覆盖映射，共 40 个不同表情。
- 补齐“满头问号”图片 CDN 的精确主机白名单，下载与重定向使用一致校验；连续表情、文字混排走现有分段渲染，未知表情或图片不可用时保留文字。
- 同类排查补齐斗鱼漏迁移的 250 个默认表情，连接前加载字典。
- 详细：[1.0.3 发布说明](docs/release-notes-1.0.3.md)。

### 1.0.2 修复（历史）

- **播放中反复「缓冲」排查**：只读取证发现用户连接稳态 1.0002x、无重连；慢段（最低 0.22–0.37x）来自共享上游网络路径（本机虎牙流量经 Clash TUN），播放器 2 s 级读缓存因此欠载。上游追上直播边缘时会在同一条 HTTP 响应内产生 +114.5 s 时间戳跳变，1.0.1 原样透传会让播放器重建分离器缓存、出现第二次缓冲。
- **中转稳健性修复**：同一响应内大时间戳跳变改为音视频共同时钟重基（+20 ms 连续续接，`relay.log` WARN，状态接口新增 `timeline_jumps`）；播放器写超时 5 s → 15 s（对齐旧版发送超时）；正常媒体分段结束后的重连等待 1 s → 150 ms。
- 详细：[1.0.2 发布说明](docs/release-notes-1.0.2.md)。

### 1.0.1 修复（历史）

- **抖音弹幕图片表情显示修复**：Rust 版此前只在 B站拉取房间表情字典，抖音的 `[捂脸]` 一类方括号表情码从未被换成图片。现在连接抖音时后台请求 `emoji/list`，解析真实表情 URL 并写入与 B站共用的表情缓存；覆盖层按缓存代次刷新，换房/换平台也会更新字典。
- 抖音表情图实测为 **VP8L 无损 WebP**（96×96），走既有 `image`/`image-webp` 解码与有界远程图片路径；拉取失败、响应超限或解码失败只回退为文字，不影响弹幕接收。
- 本地离线渲染证据支持直接读取抖音 `emoji/list` 响应形状（`LIVE_HUB_EMOTICON_JSON`），便于回归验证。
- 详细：[1.0.1 发布说明](docs/release-notes-1.0.1.md)。

### 1.0.0 修复（历史）

- 头像在后台检查尺寸和解码预算并缩为 48px；账号候选只向界面传递小图，候选缓存和失败重试均受限。
- 头像下载成功或失败立即释放名额，切换查询不再使旧头像占住后续请求。
- 新查询命中缓存也会复位上一轮核验；普通取消同步取消并复位 AI 状态。
- 头像缓存图片缺失、损坏或异常时移除失效索引，允许自动重新下载。
- 详细：[1.0.0 发布说明](docs/release-notes-1.0.0.md)。

### 0.3.15 修复（历史）

- **收藏页右侧「正在观看 / 最近打开」**：去掉了把最新会话排到最后的多余倒序（正在看的会话现在排最前）；每行向 ScrollArea 申报高度，列表溢出时出现滚动条、滚轮可滚（之前内容高度≈0，既没有滚动条也滚不动）；同一房间多次打开按 `Platform:Room` 去重，行数上限对齐原版 24 条。
- 详细：[0.3.15 发布说明](docs/release-notes-0.3.15-migration-preview.md)。

### 0.3.14 修复（历史）

- **虎牙短号房间读不到开播状态（如「安德罗妮」`a16789`）**：`mp.huya.com` 的 profileRoom 接口按短号查询返回 `status=422「该主播不存在」`，旧实现只看房间页标记，而虎牙页面把「重播中」也标成 `liveStatus-on`，无法判定只能显示「状态待确认」。现按原版 C# 的做法补上数字房号回退：先从房间页取 `"profileRoom":<数字>` 再查一次四态接口；取不到时仍如实保持「状态待确认」，不把重播/下播误报成直播。
- 详细：[0.3.14 发布说明](docs/release-notes-0.3.14-migration-preview.md)。

### 0.3.13 修复轮（历史）

- **覆盖层「有 SC 时挡鼠标 / PotPlayer 全屏底部控制栏被遮」根因修复**：主覆盖层改为永久 `WS_EX_TRANSPARENT`；SC 卡片的点击、滚轮与滚动条拖动改由独立 alpha=0 分层输入窗口承接，窗口形状用 `SetWindowRgn` 精确裁到卡片/关闭按钮/滚动条矩形——区域外像素由系统直接跳过，跨进程可靠落到播放器；鼠标在卡片上移动时仍把 `WM_MOUSEMOVE` 转发给播放器，全屏底部控制栏正常出现。
- **F7/F8/F9/F10 只触发覆盖层**：播放器前台时用 `RegisterHotKey` 动态注册（系统吞掉按键，PotPlayer 不再弹出浏览器面板/播放控制），离开前台立即注销；注册失败的键自动回退 `GetAsyncKeyState` 轮询。
- **SC 溢出时滚轮速度恢复旧版手感**：每格 72pt（等价旧版 raw delta×0.6 = 72px）；命中矩形同时裁剪到可见带。
- **主界面四角圆角**：Windows 11 DWM `DWMWA_WINDOW_CORNER_PREFERENCE=DWMWCP_ROUND`（动态加载 dwmapi.dll；Windows 10/非 Windows 保持直角，失败只记诊断不报错）。
- 详细与未验收项：[0.3.13 发布说明](docs/release-notes-0.3.13-migration-preview.md)。

### 0.3.12 修复轮（历史）

- **网页登录改回 Edge 隐私窗口（InPrivate）**：旧版用内嵌 WebView2（Edge 内核）所以没有账号同步提示；0.3.2 起为绕开同步改用了 Chrome，方向不对。0.3.5 改回 Edge，但用 InPrivate 独立隐私窗口 + 关闭同步/隐式登录特性，既贴近老版“不登录/不同步个人账号”，又保留可用的 Cookie 捕获；无 Edge 时仍回退 Chrome 独立临时资料。
- **抖音搜索结果修复的收敛处理**：空结果不再进入查询缓存（避免一次瞬时空结果被反复复用）、空/失败结果自动重试一次、搜索各平台结果写入 `<数据目录>/logs/search.log` 便于诊断；此前 0.3.4 已补齐老版完整请求参数/浏览器头（真实接口“陈泽/花小龙”均返回 10 个候选）。

### 0.3.4 抖音搜索请求修复（历史）

- **抖音“查找主播”恢复结果**：抖音官方用户搜索接口对请求头与浏览器指纹参数敏感，旧 Rust 请求缺少完整参数/浏览器头，导致检索结果恒为 0（界面无报错）。按老版逻辑补齐官方搜索页的完整查询参数（设备/浏览器指纹、pc_search_top_1_params 等）与 Edge 154 浏览器 UA/Accept/Accept-Language/Referer；真实登录态实测“花小龙”返回 10 个候选。

### 0.3.3 抖音登录 Cookie（历史）

- **抖音扫码登录保存失败修复**：浏览器登录捕获的 Cookie 里存在一个**空名称 Cookie**（Chrome 从抖音页面保留的广告/SSO 条目），旧校验遇到它直接判整次捕获失败（“Invalid cookie name”），导致明明登录成功却不保存。现改为逐条校验、跳过不可用的 Cookie 并写诊断日志（名称脱敏），其余会话 Cookie 正常保存，最后由平台健康检查确认登录；仅当全部 Cookie 都不可用时才给出明确错误。界面账号页说明文字同步去重排版。

### 0.3.2 登录浏览器隐私（历史）

- **登录浏览器不再登录/同步你的 Edge/Chrome 账号**：网页登录优先使用本机 Chrome 的全新临时资料（Chrome 没有 Windows 账户隐式登录）；仅当未安装 Chrome 才回退 Edge，并附加 disable-sync 与 msImplicitSignin/msEdgeSync/msEdgeIdentity/msEdgeFirstRunExperience 等特性开关抑制同步/首启提示；登录窗口内不再出现 Microsoft 账户同步弹窗。界面文案同步说明“全新临时资料、不会登录或同步你的账号”。

### 0.3.1 修复轮（历史）

- **用户实测修复**：主界面所有下拉框打开/选择/关闭（重写 popup 状态管理 + headless 帧测试）；收藏卡/会话行头像（B站 -352 风控 → get_anchor_in_room/Master info 回退，真实网络取到并缓存）；右上角圆钮放大（圆 20/glyph 10/间距 34）；下拉 chevron 细化；侧栏账号/观看会话图标与文字基线修正；**覆盖层弹幕右缘入场/顶部裁切根因修复**（scheduler viewport 未同步 640×240）。
- **遗留收口**：11 个可配置全局热键（设置页编辑/校验/持久化 + overlay 接线，F7–F10 保留）；窗口可见性策略接线（可见即显示/AlwaysOnTop/HideWhenNotForeground）；SC P1（badge/compact/scale200/face_frame/message_trans）；事件卡；固定弹幕水平感知避让；合并 ×N 放大；滚动条拖动；re:/uid:/name:/关键词 规则（屏蔽/高亮/置顶）；覆盖层/中转/panic 日志；URL 语法（m.douyu/www.douyin/blanc/h5）；搜索详情字段；界面字体/字号设置；收藏批量导入导出与原版数据导入。
- **已知差异/未验证**：彩色 emoji 精灵图 PLAIN 区段偏移（原版资产生成链问题，下一轮）；真实全局热键/鼠标拖动/真实桌面下拉与原生对话框/多显示器/付费 SC/真实登录需真实桌面验收；浅色主题个别对比度。详见 [0.3.1 发布说明](docs/release-notes-0.3.1-migration-preview.md)。
- 详细验证与未验收项：[验证报告](docs/validation.md)；功能差异：[对照清单](docs/parity-gap.md)。

### 0.3.0 基线（历史）

- **工作台 UI 按原版权重做**：无边框 34px 自绘 chrome（品牌/提示/红黄绿圆钮/拖动/双击最大化）、164px 侧栏（观看 / 设置与账号两组、8 个 148×36 导航、整行亮橙选中）、23px 标题 + 12px 副标题、32px 页脚；默认 1240×720 逻辑、最小 760×520。8 页全部按原版结构重写：我的收藏（launch 卡 + 3 列收藏卡网格 + 右栏"正在观看/最近打开"）、最近播放、查找主播、观看会话、播放器与画质、弹幕与 SC 设置（5 tab）、账号与登录（四平台 2×2 卡）、本地数据 / 平台状态（两 tab + 数据表）。
- **设计系统**：深/浅两套调色板（surface #101217 / card #1B1E27 / accent #7962DF / selection #FFC53D 等，逐项对齐原版 ModernTheme）；字体运行时从系统加载 Microsoft YaHei UI（msyh/msyhbd 的 TTC face 自解析）+ Segoe UI，缺失回退内置 HarmonyOS Sans SC，不打包系统字体。
- **弹幕行为**：速度改为原版"8 秒穿屏/速度百分比 + 逐条按 (视口宽+消息宽) 计算"；轨道高按文字/表情实测、全轨迹碰撞（PathsOverlap 等价）；顶部/底部/逆向固定弹幕 4s 与区域限制、超宽转滚动；拥挤缩小/准入/短队列；**窗口 resize/DPI/全屏时已飞弹幕保持轨道与进度（修复用户报告的"缩放后弹幕被压缩到一行"）**；左侧快捷键提示行保留并新增 show_shortcut_hint 开关；三档描边随字号缩放、color emoji/图片表情尺寸与基线。
- **SC 绘制**：full/compact/badge 三形态与 off、服务端两段式配色、270×scale 宽、圆形头像、价格「N元」、正文换行、白色进度轨在卡片下方、× 关闭、四角/60–200% 缩放/时长倍率、寿命=服务端 duration；ScMaxStack 改为原版"滚动阈值"语义；点击切形态/滚轮翻页/选择性命中穿透。
- **播放呈现**：mpv 标题/OSD hint 全量（anchor / media-title 分离 / bitrate / state / relay / autoreplay、script-opts 接线、斗鱼名义码率 + relay onMetaData 实测、B站 10s watcher）。
- **功能补齐**：四平台收藏卡头像解析与磁盘缓存（FAV-04，匿名公开接口，失败 20 分钟重试）、深色模式持久化、收藏页键盘操作（Enter/Shift+Enter/双击/方向键）、设置热更新（覆盖层每秒读 state.json mtime）、ScClickable、快捷键提示开关等。
- **发布前已知差异/未完成（下一轮）**：11 个可配置全局热键与窗口可见性策略（模型/单测已就绪，overlay 接线未完成，实际仍 F7–F10 固定键且"播放器前台才显示"）；固定弹幕在有 SC 时的避让偏保守；事件卡视觉、滚轮拖动、头像框、message_trans 展示；re:/uid:/name: 规则、关键词高亮/置顶；搜索候选详情字段、原版数据导入入口、导出收藏、崩溃/中转日志；m.douyu.com / www.douyin.com / blanc 等 URL 语法。

## 历史版本变化（0.2.x）

## 本版变化（0.2.3）

- 弹幕/SC 富媒体与布局：勋章/等级/守护、图片表情与大表情、SC 服务端配色与头像、四形态/四角/缩放/进度条、固定/逆向弹幕与安全区避让、短队列与拥挤缩小、文本表情字典与彩色 emoji 本地加载；设置页接线全部新字段（旧配置兼容）。
- 账号与工作台：登录健康时效（有效 10 分钟转“上次有效，待复查”、手动 10 秒下限、凭据变更作废旧结果）、新增本地数据与平台状态页、收藏/最近播放开播徽标与选中即查、会话画质/缓冲列与重开保留原配置、本地数据/平台状态与诊断。
- 播放：mpv 标题/控制条 Lua 脚本（窗口标题含分辨率/fps/编码/码率，底部标题带时间）与重连/下播 OSD、重播取流语义、便携数据布局（exe 旁 bin/mpv 或 portable.marker 时写 data\）、stream-info 与 ui-probe 诊断入口、脱敏有界运行日志。
- 性能：C# vs Rust 冷启动+离线自检 8.75 倍；中转小 tag 127.6 倍、32KiB tag 3.08 倍、FLV 解析 1.71 倍（测量与复测脚本见发布说明）。
- 0.2.3 收尾：SC 选择性命中（仅 SC 卡片接收鼠标，其余穿透；点击切形态/关闭/滚轮翻页，不抢焦点）；彩色 emoji 精灵图随包（exe 旁 assets/ 自动加载，大图切 2 张 ≤2048 纹理）；抖音网页辅助（独立浏览器+CDP 捕获、健康核验、DPAPI 保存、搜索自动重查、退出清理）；虎牙/抖音真实链路错误分类（离线房、风控、登录）；发布流水线 Publish-Release.ps1（lite/full ZIP、portable.marker、隐私门禁、签名如实 unsigned）；长时真实网络 Stress-Danmaku（3 平台×120s，0 失败）；stream-info 斗鱼时间戳修复。
- 0.2.4：设置页播放器/工具路径支持「浏览…」原生文件对话框、「自动检测」与保存前校验（相对路径/不存在/过小文件不再静默保存）。
- 详细变更与未验收项：[0.2.4 发布说明](docs/release-notes-0.2.4-migration-preview.md)（0.2.3/0.2.2 说明保留供追溯）。

真实验收已覆盖（0.2.0 记录）：B站扫码及官方登录有效、登录后的弹幕接收；Rust 启动器 → 中转 → mpv 实际视频窗口 → SC/弹幕，窗口与全屏穿透；PotPlayer 的真实视频与弹幕（用户确认）。0.2.2 新增能力（弹幕富媒体/布局、登录健康时效、本地数据页、开播徽标、重播语义、标题 Lua、性能优化等）目前只有离线/合成与性能测量证据，真实平台房间、真实付费 SC、真实重播与窗口观感仍需验收；其他平台真实登录、付费 AI、PotPlayer 全屏穿透与多显示器仍未验证。

## 运行

双击 `live-hub-rust.exe` 打开界面，在设置页指定自己安装的 mpv / PotPlayer。程序包不附带播放器或 yt-dlp。

默认数据目录为 `%LOCALAPPDATA%\LiveHubRust`。已有 Rust 数据可用 `--data-dir <目录>` 指定；不要将个人数据放入待发布目录。

```powershell
live-hub-rust --data-dir <独立数据目录> gui
live-hub-rust parse 'https://live.bilibili.com/123'
live-hub-rust play bili:123 --player mpv --with-overlay
live-hub-rust play huya:example --player potplayer --with-overlay
live-hub-rust search huya '合成查询词'
live-hub-rust auth status
live-hub-rust auth check bili
live-hub-rust import <显式选择的JSON文件> [--history]
live-hub-rust self-test
live-hub-rust capabilities
```

`parse` 与 `self-test` 不联网或打开播放器。取流、搜索、登录健康与播放会访问指定平台。Windows release 使用 GUI 子系统；采集 CLI 输出时请显式等待进程并重定向输出文件。

## 构建和验证

需要支持 edition 2024 的 Rust stable 与 Windows GNU 工具链。隔离工具链可通过环境变量配置，无需修改系统 PATH：

```powershell
$env:LIVE_HUB_RUST_TOOLCHAIN = '<隔离工具链目录>'
pwsh -File scripts/Build.ps1 -TargetDirectory '<纯ASCII构建目录>'
pwsh -File scripts/Test.ps1 -TargetDirectory '<纯ASCII构建目录>'
pwsh -File scripts/Test.ps1 -TargetDirectory '<纯ASCII构建目录>' -NoGui
pwsh -File scripts/New-Package.ps1 -TargetDirectory '<纯ASCII构建目录>' -OutputDirectory '<新程序包目录>'
pwsh -File scripts/Test-Package.ps1 -Path '<新程序包目录>'
```

默认测试使用合成数据与本机回环，不打开真实播放器、不接触账号、不访问直播平台。Windows 端口所有权测试需要可读取系统 TCP 表的执行环境；受限环境应报告限制，不跳过身份校验。

真实验收与长时压测是显式入口：

```powershell
pwsh -File scripts/Acceptance-Production.ps1 -Exe <测试EXE> -Mpv <mpv.exe> -EvidenceDirectory <本机证据目录> -Windowed -SyntheticOverlay
pwsh -File scripts/Stress-Relay.ps1 -Mode synthetic-soak
```

窗口验收会新建静音测试窗口，应先安排可占用前台的时间。诊断输出和二维码属于本机资料，不随程序包分发。

## 实现边界

原版的表情包组合细节（多表情包并存/自定义表情）、徽章/头像与复杂 SC 样式、完整网页辅助、全部重播内容、所有窗口/画质设置以及 full/lite 正式发布流水线仍未对齐。

依赖版本固定在 Cargo.lock。原项目许可、字体许可、平台算法与 Cargo 依赖原文随包保留，见 [第三方声明](THIRD_PARTY_NOTICES.md)。本地验收不代表已完成 GitHub 发布审核。
