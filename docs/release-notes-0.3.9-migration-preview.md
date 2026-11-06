# 0.3.9 本地迁移预览（MPV 累计音画不同步 / 斗鱼登录态播放 / PotPlayer 抖音诊断）

更新：2026-10-31。基线 0.3.8；数据与旧版兼容。

## 用户反馈与修复

### 1. MPV 播放越久越音画不同步（累计误差）
根因：旧版 mpv 参数只设了 --video-sync=audio，没设 --autosync。mpv 默认 --autosync=0 会完全按音频延迟测量值决定帧时序；部分声卡/驱动的延迟测量有固定误差，这个误差会持续累积到视频时钟，表现为“越播越不同步”。网络抖动/缓存不会造成持续累计（只会造成固定延迟或瞬时卡顿），累计偏移更像音频时钟/延迟测量问题。
修复：给 mpv 增加 --autosync=30（mpv 文档针对延迟测量不完美声卡的推荐值：突变偏移约 1–2 秒内收敛，而不是无限漂移），保留 --video-sync=audio 与 --framedrop=vo。若用户 mpv.conf 强制覆盖 autosync，命令行参数仍优先（额外参数在其后，用户自定义可覆盖）。

### 2. 斗鱼显示“平台要求登录或网页验证”但 Cookie 正常（mpv/PotPlayer 都不播）
根因：斗鱼取流走 getEncryption → getH5PlayV1 匿名 H5 接口；旧 Rust 在已登录会话下把斗鱼 Cookie 一起发了过去，而请求里的合成 did 与登录态不匹配，斗鱼返回 401/403 → 被映射成“需要登录/验证”。用同一 Cookie 库实测：带登录态失败、匿名成功。
修复：斗鱼取流 API（getEncryption / getH5PlayV1 / betard 探测）改走不带 Cookie 的公开请求（固定 host 白名单），搜索/核验等需要登录的请求保持原样。真实环境实测 douyu:633019 已能解析出直播流地址。

### 3. 抖音用 PotPlayer 一直“正在打开”
本机用真实抖音直播房间（926530112088 / 768392702185）通过 CLI 启动 Rust 中转 + PotPlayer，PotPlayer 正常播放（窗口标题与画面均正常），未复现必现卡死；因此更可能是房间/瞬时中转/播放器实例状态相关。0.3.9 补齐中转日志定位：relay 子进程现在跟随启动器真实 --data-dir 写 logs/relay.log（此前可能落到默认数据目录），并记录播放器生命周期注册。若仍卡住，把 <数据目录>/logs/relay.log 发来即可区分“上游解析失败/播放器未注册/连接未建立”。

## 验证

- cargo check --all-targets EXIT=0；cargo test --lib 307 passed / 0 failed / 2 ignored。
- 真实：斗鱼带登录态 resolve 由失败变成功；抖音两个直播房间 PotPlayer 实测可播放；mpv 参数含 autosync=30（需用户实机长时间播放验证累计偏移是否消失）。
- 两矩阵/包复核见 docs/validation.md 与 notes/verification-r9.md（verifier 填）。

## 限制

- 音画不同步若仍出现，需确认用户 mpv.conf/extra args 没有覆盖 autosync，或换 --video-sync=display-vdrop 做 A/B；可在设置 mpv 额外参数里加 --autosync=30 复测。
- PotPlayer 卡死需要 relay.log 或具体房间号才能进一步定位。
