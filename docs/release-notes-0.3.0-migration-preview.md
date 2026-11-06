# 0.3.0 本地迁移预览（UI 还原 + 弹幕/SC 对齐 + 播放呈现补齐）

日期：2026-10-31（本地迁移预览，未发布远端）
基线：0.2.4（保留 `artifacts/preview-0.2.4` 作为回退）

## 本版重点

### 1. 工作台 UI 按原版权重做

- 无边框窗口 + 34px 自绘 chrome（品牌图标 / "看直播" / 四平台提示 / 红黄绿 TrafficDot），支持拖动、双击最大化、最小化/关闭；默认 1240×720 逻辑、最小 760×520。
- 设计系统 1:1 移植原版 ModernTheme：深/浅两套调色板（surface #101217 / card #1B1E27 / border #2C3140 / accent #7962DF / selection #FFC53D 等）、字号/行高/圆角/描边、无投影；B站/虎牙/斗鱼/抖音平台色。
- 字体对齐原版：运行时从系统字体目录加载 **Microsoft YaHei UI**（msyh.ttc / msyhbd.ttc 的 TTC face 自解析）与 **Segoe UI**；缺失/失败安全回退内置 HarmonyOS Sans SC 与 egui 默认字体，不 panic、不打包系统字体。
- 8 个页面按原版结构重写：**我的收藏**（顶部 launch 工作台：平台 pill / 房间输入 / 格式说明 / mpv·PotPlayer·取消启动 / 画质·mpv 缓冲·弹幕·SC·画质增强 / 本次与保存范围提示 / 弹幕设置；收藏卡网格 3 列 + 状态 pill + 选中态 + 双播放器按钮；右栏"正在观看 / 最近打开"会话卡）、**最近播放**、**查找主播**、**观看会话**、**播放器与画质**、**弹幕与 SC 设置**（5 个 tab：显示与内容 / SC 醒目留言 / 过滤与开关 / 更多弹幕设置 / 界面与热键）、**账号与登录**（四平台 2×2 卡片）、**本地数据 / 平台状态**（两 tab + 表格 + 维护 + 构建信息）。
- 交互：Enter mpv / Shift+Enter PotPlayer / 双击卡片 / 选择回填 / 平台切换 / 收藏与删除 / 会话重开 / 搜索流程 / 设置保存 / 账号 Cookie 状态 / 数据清理 / Tab 与焦点。
- 保留 Rust 侧优点：数据页只读明细与日志过滤、诊断/观测、解析工具路径、播放恢复设置等在原版框架内保留（"取长补短"）。

### 2. 弹幕行为对齐（用户实测反馈）

- 滚动速度改为原版语义：穿屏时长 = 8s / (速度% / 100)，逐条速度 = (视口宽 + 消息宽) / 时长（长弹幕更快）。
- 轨道高度按文字+表情实测（ceil(max(font*1.33+6, font*1.18+6)) × 行距%），碰撞改为全轨迹判交（legacy PathsOverlap 等价）+ 入口间距。
- 顶部 / 底部 / 逆向固定弹幕：4s 寿命、超宽转滚动、上下各 40% 区域、每类上限；拥挤缩小按 densityBudget=min(同屏上限, 轨道数×2) 80% 触发、舰长/守护豁免；短队列平滑准入/每秒上限/等待与容量。
- **窗口 resize / DPI / 全屏切换**：已在飞弹幕保持轨道与飞行进度（按新宽度重算速度，不重置不压行）——修复用户报告的"缩放后弹幕被压缩到一行"。
- 三档描边随字号缩放、透明度、行距、图片表情/彩色 emoji 尺寸与基线、勋章/等级/守护/用户名前缀、事件卡（礼物/上舰/进场/公告）。
- 左下快捷键提示行（F7/F8 字号 · F9 弹幕 · F10 SC）：**保留**，新增 **show_shortcut_hint 开关**（默认开，可在弹幕设置关闭）。

### 3. SC 绘制对齐（用户实测反馈）

- full / compact / badge 三形态与 off：服务端两段式配色、宽 270×scale×uiScale、头像圆裁切、昵称/「N元」价格、正文换行、卡片下白色进度轨 + 同色进度、右上 × 关闭（含命中）、四角位置与边距、60%–200% 缩放、停留时长倍率、寿命改为服务端 duration（去掉 8s 上限）。
- ScMaxStack 改为原版"滚动阈值"语义（不淘汰、超出滚轮翻看，硬顶 200）；点击卡片循环形态、点击 × 删除、滚轮翻页；命中仅卡片区域、其余像素穿透播放器。
- 设置字段向后兼容（serde default，旧 state.json 可加载）。

### 4. 播放呈现与标题 hint

- mpv 标题/OSD hint 按原版补齐：anchor / media-title（直播间标题分离）/ bitrate / state（LIVE/OFFLINE/REPLAY/LOOP/RECORDING/UNKNOWN±ticks）/ relay（ONLINE/RECONNECTING/OFFLINE/REPLAY-UNAVAILABLE）/ autoreplay，script-opts 全量接线；live-title.lua 拼「主播 - 标题 · 分辨率 fps 编码 码率」+ 底栏日期时间 + 房态提示。
- relay 从首个 FLV onMetaData 解析码率（零额外连接）；斗鱼名义码率 + 实测兜底；B站 10s watcher 有界退避。

### 5. 功能对等补齐（本轮已落地）

- 账号页四平台卡片（Cookie 状态/最近检测/原因/扫码/检测/详情/退出）；播放器与画质页（程序路径 + 4 项偏好 + 解析工具/播放恢复）；数据页表格与平台状态；搜索页查询/动作行/平台过滤/候选详情与来源/收藏行；全局键盘（Enter/Shift+Enter/双击/方向选择）。
- 设置热更新：覆盖层每秒读 state.json mtime，合法则应用（失败保留旧值）；ScClickable 开关；深色模式持久化。
- 收藏卡平台头像（FAV-04）：四平台匿名头像 URL 解析 + 磁盘缓存（48px PNG、原子写、失败 20 分钟重试）+ UI 纹理接线；离线首字占位，真实网络待用户验收。
- 键盘：收藏页 Enter/Shift+Enter/双击/方向键/Esc；主题持久化；`--ui-shot` 离屏截图诊断与逐页对比工具链。

## 已知差异 / 未完成（下一轮）

- **11 个可配置全局热键**：`src/hotkeys.rs`（accelerator 解析/注册/降级）+ core HotkeyBindings + 契约测试 6/6 已完成；**overlay 与设置页接线未完成**，实际仍只有 F7–F10 固定键（用户要求的提示行保留并加开关已实现）。
- **窗口可见性策略**：`src/window_policy.rs` 真值表 + 单测完成；**overlay 未消费**，默认仍是"播放器前台才显示"（原版为播放器可见即显示）。
- 固定弹幕在存在 SC 时避让偏保守（垂直整带屏蔽）；SC 收尾：头像框 face_frame、message_trans 展示、滚动条拖动、事件卡视觉、合并弹幕 ×N 放大。
- 搜索候选详情字段（platform_update/live_status/sec_uid/verified_at/match_reasons/sources 等）；re:/uid:/name: 规则与关键词高亮/置顶；界面字体/字号设置与重启按钮；原版数据导入入口；导出收藏；崩溃/中转/覆盖层日志。
- URL 语法：m.douyu.com、www.douyin.com、live.bilibili.com/blanc 等原版可识别格式。
- 浅色主题部分默认态（placeholder 对比度/页脚状态行）仍有差异；收藏卡真实头像与开播 pill 需要真实网络。

## 验证

- 离线回归（冻结树，2026-10-31）：默认 features **510 passed / 0 failed / 15 ignored**（EXIT=0）；--no-default-features **338 passed / 0 failed / 14 ignored**（EXIT=0）；ignored 均为显式真实窗口/压力 harness。详见 docs/validation.md §0.3.0-matrix。
- UI 对照：原版真实窗口截图（1240×720@200% = 2480×1440）逐页并排/差分，见任务产出的 output/ui-compare（MAE/changed%）。
- 覆盖层：原版官方 demo 帧（SC 三形态/四角/缩放/滚动/emoji）与 Rust 合成场景截图对照。
- 真实环境未验证项：真实付费 SC、全屏 mpv/PotPlayer 穿透、多显示器/混合 DPI、24h 长时/休眠/网络切换、其他平台真实登录、真实收藏头像等，见 docs/validation.md。

## 升级与回退

- 数据：默认数据目录不变；旧 state.json 字段兼容（新字段 serde 默认）。不读取原版 C# 数据或浏览器 profile。
- 回退：保留 0.2.4 预览包；将 `artifacts/current` 指回 0.2.4 或直接运行旧包内 exe。