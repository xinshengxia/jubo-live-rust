# 0.3.10 本地迁移预览（PotPlayer 标题参数 / 抖音 CDN 跳转 / MPV 长时间音画同步）

更新：2026-10-31。基线 0.3.9；数据与旧版兼容。

## 用户反馈与修复

### 1. PotPlayer 窗口标题少了后半段参数
旧 Rust 版 PotPlayer 只传了解析出的基础标题，原版 C# 会在拉起前探测 FLV 首包 onMetaData，并把“宽x高 fps 编码 码率”拼到标题后。0.3.10 已补齐：通过已启动的中转读一次 FLV header/onMetaData（不额外占用上游连接），拼成与 mpv/原版一致的标题，例如“主播 - 标题  ·  1920x1080 62fps h264 5880kbps”。本机实测 PotPlayer 标题已带完整后缀，且不影响播放。

### 2. 抖音“安德罗妮学妹”房间主程序显示打开成功但看不到画面
根因不在播放器：抖音直播 FLV 会 302 跳到字节系 *.jomodns.com CDN（例如 rt2016n.jomodns.com/pull-flv-q13.douyincdn.com/...），中转的媒体 CDN 白名单只允许 douyincdn.com 等，拒绝了这条重定向，于是播放器拿不到任何数据、主程序却已先报启动成功。0.3.10 把观察到的 jomodns.com 加入受信媒体 CDN 白名单（仍要求 HTTPS、公网 DNS、固定地址与重定向次数限制），并保留原有路径/DNS 校验。本机实测该房间 mpv/PotPlayer 均恢复正常播放。

### 3. MPV 长时间音画不同步（已对照 jubo-live 各 tag）
逐 tag 核对了 jubo-live 的 mpv 参数历史：
- v1.0.3 / v1.0.4：只有 --hwdec=auto-safe、缓存/画质增强，没有 --video-sync 与 --framedrop（使用 mpv 默认 audio 同步）；
- v1.0.5：改为 --video-sync=display-resample（把视频锁到显示器并重采样音频）；
- v1.0.6 起（含 v1.0.7、v1.1.x）：改为 --video-sync=audio --framedrop=vo，并沿用至今。
因此“较早没有 bug”的版本对应 v1.0.4 或更早；0.3.10 把 mpv 拉起参数恢复为 **v1.0.4 兼容形态**（不强制 --video-sync/--framedrop，mpv 默认仍为 audio 同步），并额外保留 --autosync=30 作为声卡延迟测量误差的安全网（mpv 文档：30 时突变偏移约 1–2 秒内收敛；默认 0 会直接采用有误差的测量值）。需要 forced framedrop 或 display-resample 的机器，可暂时在设置「mpv 额外参数」里自行加回做 A/B。

## 验证

- cargo check --all-targets EXIT=0；cargo test --lib 309 passed / 0 failed / 2 ignored（新增 PotPlayer 标题拼接、jomodns 白名单测试）。
- 真实：抖音 796794251979（安德罗妮学妹）通过中转 + PotPlayer 正常播放，窗口标题含“1920x1080 62fps h264 5880kbps”；mpv 同样可播。
- 两矩阵/包复核见 docs/validation.md 与 notes/verification-r10.md（verifier 填）。

## 限制

- MPV 长时间音画同步仍需用户实机连续播放 20–30 分钟确认；若仍有小偏移可尝试 --video-sync=display-vdrop 做 A/B。
- 未来若抖音再出现新的 302 CDN 家族，需要把具体 host 加入白名单并补测试，不能用通配“放行一切 CDN”。
