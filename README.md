# jubo Live Rust

「看直播」的 Rust 版本，面向 Windows x64。支持 B站、虎牙、斗鱼和抖音，使用 mpv 或 PotPlayer 播放，提供弹幕 / SC 覆盖层、主播搜索、账号登录、收藏、历史和多播放会话。

**当前版本：1.0.3。** [下载 Windows 程序包](https://github.com/xinshengxia/jubo-live-rust/releases/latest) · [C# 原项目](https://github.com/xinshengxia/jubo-live)

## 下载和使用

1. 在 Releases 下载 `jubo-live-rust-1.0.3-win-x64-lite.zip`，解压到可写目录。
2. 双击 `live-hub-rust.exe`，在设置中选择自己安装的 mpv 或 PotPlayer。
3. 输入房间号或官方直播间链接，选择画质后打开；登录、收藏与弹幕设置在对应页面管理。

lite 包不附带 mpv、PotPlayer 或 yt-dlp，播放器由用户自行安装。程序未做代码签名。

## 功能

- 四平台取流与房间状态查询、分平台及跨平台主播搜索。
- 本机 FLV 中继、断线重连和独立播放会话。
- 透明弹幕 / SC 覆盖层、过滤与合并、字号与布局设置、快捷键。
- B站扫码与独立浏览器辅助登录；Cookie 和 AI 密钥使用 Windows DPAPI 加密保存。
- 收藏与历史、本地数据管理、收藏导入导出。
- 可选 DeepSeek 联网研究，需用户自行配置 API Key 并主动触发，可能产生服务费用。

## 数据与隐私

发布包没有账号、Cookie、API Key、收藏、观看历史或浏览器资料。便携版首次运行后才在包内 `data/` 创建个人数据；源码运行默认写入 `%LOCALAPPDATA%/LiveHubRust`，也可显式指定 `--data-dir`。更新时保留自己的数据目录，不要把个人数据提交到 Git。

Rust 版与 C# 版使用独立数据目录。需要迁移收藏时，由用户主动导出、导入；不要直接复制旧程序的账号目录。

## 1.0.3 修复

- 修复虎牙 `[666]` 等表情显示成原始方括号文字的问题，连接前加载 40 个默认及已知补充表情。
- 同类排查补齐斗鱼漏迁移的 250 个默认表情；连续表情和中文混排使用同一图片渲染路径。
- 精确补齐虎牙额外图片 CDN 主机校验；未知表情、下载中或图片不可用时保留文字。
- 本版不覆盖全部服务端、付费及房间专属表情包。详见 [1.0.3 发布说明](docs/release-notes-1.0.3.md)。

### 1.0.2 修复（历史）

- **播放中途反复「缓冲」的中转侧修复**：只读观测确认中转在健康时段按实时速率转发（约 103 tag/s，媒体时间与墙钟约 1.0 倍），出现「缓冲」的时间段对应上游网络短时降速。上游恢复并跳到直播边缘时，会在同一条 HTTP 响应内产生多秒的音视频时间戳跳变；旧版中转原样转发会让播放器把它当作流重启、重建分离器缓存，造成第二次缓冲。现在这类跳变由中转重基为连续时钟（音视频共用修正，跳变后从已转发时钟 +20 ms 续接；被上游跳过的内容仍然缺失，但播放器时钟不断裂），并在中转日志和状态接口记录次数。
- 播放器写超时从 5 秒放宽到 15 秒（对齐旧版启动器的发送超时）；上游正常媒体分段结束后的重连等待从 1 秒缩短到 150 毫秒，减少播放器无数据窗口。
- 上游带宽型降速无法由程序消除，本版只缩短可恢复窗口并避免播放器时钟断裂；使用小缓存播放器（如 PotPlayer 默认配置）时仍可能受上游网络短时降速影响。

### 1.0.1 修复（历史）

- **抖音弹幕图片表情**：Rust 版此前没有拉取抖音 `emoji/list` 表情字典，`[捂脸]` 一类平台表情码按原始文字显示。现在连接抖音时在后台拉取并缓存整份字典，覆盖层按缓存代次刷新后把表情渲染为图片（真实表情为 VP8L 无损 WebP，96×96）；换房间或切平台也会更新字典。拉取或解码失败只退回文字，不影响弹幕接收。
- **表情字典离线验证入口**：本地 `LIVE_HUB_EMOTICON_JSON` 现在同时接受抖音 `emoji/list` 与 B站 `GetEmoticons` 两种响应形状，便于离线回归。
- 另修正 1.0.0 遗留的一条全量测试契约断言（能力清单 stage 已是 `release`），不涉及用户功能。

### 1.0.0 修复（历史）

- 头像解码前限制尺寸和分配，后台生成 48px 缩略图；图片与纹理缓存设有上限。
- 头像下载完成立即释放名额，切换查询不再阻塞后续头像。
- 命中搜索缓存也复位旧核验状态；取消查询同步结束 AI 的忙碌状态。
- 缓存头像缺失或损坏后自动移除失效索引，允许重新下载。
- 公开发行的彩色 emoji 图集使用 Noto Emoji 开源字体重新生成，保留原许可和来源。

## 构建

需要支持 Rust edition 2024 的 stable 工具链和 Windows GNU 构建环境。依赖版本固定在 `Cargo.lock`。

```powershell
cargo build --release --locked
cargo test --locked --lib -- --test-threads=1
```

也可使用 `scripts/Invoke-Cargo.ps1` 指定独立工具链。Windows GNU 链接器的输出目录需为纯 ASCII 路径，例如通过 `CARGO_TARGET_DIR` 或 `-TargetDirectory` 指定。

```powershell
pwsh -File scripts/New-Package.ps1 -ToolchainRoot '<toolchain>' -TargetDirectory '<ascii-build-directory>' -OutputDirectory '<new-package-directory>'
```

默认测试使用合成数据和临时目录。需要主机 TCP 端口所有权权限的测试及需要 DNS 的测试，在受限环境可能不可用；真实平台、真实登录、付费服务与 GUI 人工验收分别记录，不能由离线测试代替。详见 [验证范围](docs/validation.md)。

## 许可与第三方组件

项目沿用 GPL-3.0；完整原文见 [LICENSE](LICENSE)。第三方代码、协议参考、字体和表情素材分别适用原许可，见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 和 `licenses/`。

直播内容、平台账号、头像和在线表情的权利属于相应权利人。请遵守平台条款，仅在有权使用的范围内使用本程序。
