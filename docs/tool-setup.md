# 工具配置与获取

## mpv

默认播放需要 **OpenAL 音频输出**。应用直接探测选中的二进制，不根据文件名、“完整版”或版本新旧推断支持。没有 OpenAL 时请换构建或选择 PotPlayer；应用不会静默切回其他后端。

2026-11-17 已检查的 Windows x64 构建：shinchiro `20261021`，mpv `v0.41.0-1227-ge471496c5`；版本输出及 `--ao=help` 包含 OpenAL 已通过。本地短播放、实际直播和两小时同步结果见本版验证记录，未完成部分不作保证。

[下载该兼容构建](https://github.com/shinchiro/mpv-winbuild-cmake/releases/download/20261021/mpv-x86_64-20261021-git-e471496c56.7z) · [构建发布页](https://github.com/shinchiro/mpv-winbuild-cmake/releases/tag/20261021)

归档 SHA-256：`4a355f3e89c83fabc730d33fb4e759dfec747806ed2287b61a90d1681b83e8d3`。解压后选择 `mpv.exe`。该条是已检查的兼容选择，不表示它是最新 mpv，也不会强制替换用户现有播放器。其他构建须在应用里重新检测。

## PotPlayer

[官方入口](https://potplayer.daum.net/)。选择 `PotPlayerMini64.exe` / `PotPlayerMini.exe` 或直接指向它的快捷方式。PotPlayer 不走 mpv 的 OpenAL 检查，程序身份检查不等于实际起播或长时同步已通过。

## yt-dlp

官方 GitHub 项目：**[yt-dlp/yt-dlp](https://github.com/yt-dlp/yt-dlp)** · [官方 Releases](https://github.com/yt-dlp/yt-dlp/releases)。

源码调用链：`platforms::resolve_with_hints` 中，只有 B站原生解析出现 Transport / InvalidResponse / ReplayUnavailable 等部分失败且已配置工具时才调用 yt-dlp。虎牙、斗鱼、抖音直接使用各自适配器，搜索及弹幕也不以 yt-dlp 为前置条件。

已有程序：选择 `yt-dlp.exe` / 快捷方式，或粘贴路径后检测并保存。自动发现检查程序目录、包内 bin、PATH、常见安装目录，包括每用户 `Programs/mpv` 和 `Programs/yt-dlp`，不递归扫描磁盘。

没有时：“下载并配置”会先显示组件、用途、版本、约 23.6 MB 大小及来源，用户触发后下载。受维护清单固定 Windows x64 **2026.10.26**，来源 [yt-dlp 官方 Release](https://github.com/yt-dlp/yt-dlp/releases/tag/2026.10.26)，官方资产大小 23,575,744 字节，SHA-256 `f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d`。

仅允许官方 HTTPS 分发跳转，完整校验后才启用。后台进度、取消和重试入口保留；临时文件不作为安装成功，失败不替换旧版本。保存到本应用数据目录的 tools 子目录，不改系统 PATH、不要求管理员、不安装 Deno/FFmpeg。固定清单需要应用后续维护；它不声称永远最新。

程序或快捷方式导入统一走检测流程。脚本、网页 `.url`、安装器包装和特殊商店入口不支持。快捷方式参数与工作目录不继承；程序实际位置会回显，版本探测通过也不表示全平台解析测试通过。
