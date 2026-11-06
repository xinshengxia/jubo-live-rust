# 0.2.3 本地迁移预览

本版在 0.2.2 基础上收尾剩余可见缺口，并补齐发布流水线。

## SC 交互与覆盖层

- 选择性命中：只有 SC 卡片与关闭热点接收鼠标（WM_NCHITTEST），其余区域保持穿透给播放器；不抢焦点（WM_MOUSEACTIVATE=MA_NOACTIVATE）。
- 点击卡片在“完整/紧凑”形态间切换（角标形态直接展开）、右上角关闭单条、滚轮翻页；passthrough 样式随卡片出现/消失动态切换。
- 布局仍保留四角/堆叠/缩放/时长/头像/进度条；无窗口矩形布局证据见本机验收记录。

## 弹幕富媒体与真实链路

- 彩色 emoji 精灵图随包：exe 旁 assets/emoji-sheet.png/.txt 默认加载（LIVE_HUB_EMOJI_SHEET 覆盖保留）；1920x3360 精灵图按整 cell 行切成 2 张 ≤2048 纹理，uv 正确映射；缺图/坏图降级不崩。
- 虎牙：跟随房间页重定向；每连接读 cache 的 liveStatus/liveChannel/yyid 兜底；订阅空组时按“房间未开播”或“lChannelId 可能取错”分类（不再误导离线房）。
- 抖音：ttwid 无条件格式校验并随 webcast WebSocket 发送；房间页 401/403/captcha 立即停止重连并提示登录；风控/格式错误分类文案；12s 无推送提示可能风控。
- B站真实表情字典：新增鉴权拉取路径（隔离 DPAPI vault）+ env-gated 真实验收测试（LIVE_HUB_REAL_ACCEPTANCE=1、LIVE_HUB_REAL_DATA_DIR）；已用本机已登录隔离数据目录真实跑通：房间 7777→真实 545068，返回 54 个表情码。根因修复：真实响应包列表在 data.data（旧解析读 data.packages），现兼容两者。

## 搜索与账号

- 抖音网页辅助：用本程序独立 Edge/Chrome 资料目录打开抖音页面，用户完成登录/验证后由 CDP 只读捕获抖音域 Cookie → 官方健康核验 → 仅明确有效时 DPAPI 保存；成功后作废搜索缓存并按当前关键词自动重查；不自动选中/收藏/播放。
- 退出抖音登录并清理辅助资料：只删除本程序 browser-login/douyin-<hex> 目录，其他平台/普通文件/链接保留。
- 搜索侧对验证/风控/限流给出网页辅助提示；全部平台查询中抖音受阻时保留其他平台结果。

## 发布与稳定性

- scripts/Publish-Release.ps1：一次产出 lite/full ZIP，带 portable.marker、release-manifest.json（版本/构建时间/逐文件 SHA256/签名状态/门禁结果）、.zip.sha256 与 summary；隐私门禁（用户数据/凭据/私有路径、UTF-8+UTF-16 视图、>8MiB 分块）0 发现；ZIP 内逐项复验；拒绝 symlink/越界；默认拒绝打包 PotPlayer/yt-dlp，full 可显式带用户自备 mpv 与许可/生成 NOTICE。
- 签名：检测到 signtool 但无用户证书时如实标记 unsigned，-RequireSignature 时失败；需用户自备证书与时间戳。
- 长时真实网络稳定性：scripts/Stress-Danmaku.ps1，bili:7777 / huya:660000 / douyu:252140 各 120s：message 155/40/97、connect Ok、0 失败（有界替代 24h，差距见本机验收记录）。
- stream-info 真实 FLV：斗鱼时间戳跨度修复（douyu:252140 由 fps 0.040/span 6,088,300 修为 29.999fps/9,067ms/3280kbps；异常跳变时 reliable=false 并输出 null）。

## 回归与未验收

- 回归数字见 docs/validation.md 当次日志。
- 未验收：真实全屏 mpv/PotPlayer 下 SC 点击/滚轮命中、彩色 emoji 实机观感、其他平台真实登录与登录后弹幕、PotPlayer 全屏/SC 穿透、多显示器/混合 DPI、24 小时真实直播/网络切换/休眠、正式签名与远端发布。
