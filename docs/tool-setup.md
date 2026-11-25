# 工具配置与获取

## mpv

推荐使用 **shinchiro 最新成功的普通 Windows x64 构建**。打开作者的 Actions 构建记录，从最近的一次开始查看 `Building mpv (x86_64)` 作业。只要该作业成功且产物可用，即可登录 GitHub，在该次运行的 Artifacts 下下载 `mpv-x86_64`。其他架构可能让整次运行显示失败。[查看各日期的构建记录](https://github.com/shinchiro/mpv-winbuild-cmake/actions/workflows/mpv_clang.yml)

已核验的 2026-11-25 构建为 `v0.42.0-229-gd32c910be`，已通过 OpenAL/WASAPI 双声道静音播放测试。本次普通 x64 下载有效至 2027-02-23 00:00 UTC。此链接过期后，请从上方的构建记录选择最新可用作业。[查看本次 x64 成功作业](https://github.com/shinchiro/mpv-winbuild-cmake/actions/runs/45514553107/job/138870539617) · [下载本次 mpv-x86_64](https://github.com/shinchiro/mpv-winbuild-cmake/actions/runs/45514553107/artifacts/14979868817)

下载后先解压 Actions 提供的 ZIP，再解压里面的 `mpv-x86_64-20261125-git-d32c910be6.7z`。在应用的“播放器”页选择 `mpv.exe`，点击“检测并保存”即可检查本应用需要的 OpenAL 音频输出。

| 11 月 25 日构建的归档 | SHA-256 |
| --- | --- |
| Actions 下载的 ZIP | `091e89a2b4cc34c3ec9eca121b5a5b2d717e0cd1df3af7919673a7cdc7769f1c` |
| ZIP 内的 7z | `6b2960130007817c9be5ddfd1d3f69ca476d3d391d5549d303b7c05668cb7fad` |

## PotPlayer

[官方入口](https://potplayer.daum.net/)。选择 `PotPlayerMini64.exe` / `PotPlayerMini.exe` 或直接指向它的快捷方式。PotPlayer 不走 mpv 的 OpenAL 检查，程序身份检查不等于实际起播或长时同步已通过。

## yt-dlp

官方 GitHub 项目：**[yt-dlp/yt-dlp](https://github.com/yt-dlp/yt-dlp)** · [官方 Releases](https://github.com/yt-dlp/yt-dlp/releases)。

源码调用链：`platforms::resolve_with_hints` 中，只有 B站原生解析出现 Transport / InvalidResponse / ReplayUnavailable 等部分失败且已配置工具时才调用 yt-dlp。虎牙、斗鱼、抖音直接使用各自适配器，搜索及弹幕也不以 yt-dlp 为前置条件。

已有程序：选择 `yt-dlp.exe` / 快捷方式，或粘贴路径后检测并保存。自动发现检查程序目录、包内 bin、PATH、常见安装目录，包括每用户 `Programs/mpv` 和 `Programs/yt-dlp`，不递归扫描磁盘。

没有时：“下载并配置”会先显示组件、用途、版本、约 23.6 MB 大小及来源，用户触发后下载。受维护清单固定 Windows x64 **2026.10.26**，来源 [yt-dlp 官方 Release](https://github.com/yt-dlp/yt-dlp/releases/tag/2026.10.26)，官方资产大小 23,575,744 字节，SHA-256 `f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d`。

仅允许官方 HTTPS 分发跳转，完整校验后才启用。后台进度、取消和重试入口保留；临时文件不作为安装成功，失败不替换旧版本。保存到本应用数据目录的 tools 子目录，不改系统 PATH、不要求管理员、不安装 Deno/FFmpeg。固定清单需要应用后续维护；它不声称永远最新。

程序或快捷方式导入统一走检测流程。脚本、网页 `.url`、安装器包装和特殊商店入口不支持。快捷方式参数与工作目录不继承；程序实际位置会回显，版本探测通过也不表示全平台解析测试通过。
