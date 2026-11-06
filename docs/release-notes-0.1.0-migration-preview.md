# live-hub-rust 0.1.0 迁移预览 发布说明（草案）

- 版本：`0.1.0`
- 阶段：`migration-preview`（迁移预览，不是正式对外发布）
- 目标平台：Windows x64（GNU 工具链构建）
- 说明日期：2026-10-29
- 产物形态：本地绿色预览目录（`live-hub-rust.exe` + 文档 + 许可原文 + `manifest.json`），不含安装程序

> 本文件是发布说明草案。预览目录由 `scripts/New-Preview.ps1` 生成并自动用
> `scripts/Test-Preview.ps1` 校验；合并后的最终预览需在干净构建上重新生成后再对外分发。

## 1. 这是什么

live-hub（看直播）的独立 Rust 重构工程，与旧 C# 应用并行存在、独立数据目录、互不读取配置。
本预览提供：四平台（B站 / 虎牙 / 斗鱼 / 抖音）房间解析与取流、外部播放器（mpv / PotPlayer）启动、
本机回环 FLV 中转、四平台弹幕接收与透明覆盖层、收藏 / 历史 / 显式导入、平台搜索与显式凭据管理、
原生 egui 启动器。

这是一个**迁移预览**：用于验证重构方向与核心链路，不能当成功能与稳定性等同于原版的正式版本。

## 2. 已实现能力

- 四平台房间解析、公开搜索、播放标题查询；抖音搜索与部分接口需要用户显式导入凭据。
- mpv / PotPlayer 启动参数计划与实际启动；非 B 平台经本机回环 FLV 中转（断流重连、播放器退出即回收中转；
  中转与播放器生命周期由独立监管进程持有）。
- 四平台弹幕接收：B站、斗鱼、虎牙（Tars 子集）、抖音（纯算法签名 + WebSocket）。
  四个接收器都有基于合成夹具的离线协议测试，不包含真实平台登录态。
- 透明弹幕覆盖层：跟随播放器窗口、鼠标穿透、SC 卡片栈、四平台统一分发、播放后自动启动。
- 弹幕自动启动、字体、速度、透明度、显示区域、数量上限与 SC 开关持久化到独立设置目录。
- 独立数据目录（原子写入、并发写合并、文件锁）、收藏与最近播放、显式 JSON 导入、离线自检。
- 显式凭据：只接受用户导入的 Cookie 头或 Netscape 文本，按平台隔离、Windows DPAPI 加密，
  仅用于该平台作用域请求；HTTP / WSS 按实际目标取值，页面 host-only Cookie 不挪到其他主机。
- 离线 CLI：`self-test`、`capabilities`、`parse` 等，默认测试套件不联网、不开真实播放器窗口。

## 3. 验证状态（实际执行）

| 项目 | 结果 |
| --- | --- |
| 默认离线测试套件（`--locked --offline`，第四轮合并后） | **191 passed / 0 failed / 9 ignored**（ignored 为真实窗口/真实 mpv/真实网络/长时压测的显式验收入口与父测试 helper） |
| release 预览 EXE 离线自检 | 7/7 通过，退出码 0 |
| 启动器六页面绘制检查 | 两种尺寸下实际 egui 绘制与 tessellate 通过；另有真实窗口非激活启动验收（标题/DPI/图标/设置读回） |
| 真实 mpv 无窗口解码 | B站 / 虎牙 / 斗鱼 3/3 通过（生产同款参数，解码推进 +2.5s，无未知参数） |
| 透明覆盖层真实窗口 | 几何跟随（0 误差）、分层/穿透/NOACTIVATE/TOOLWINDOW 样式、不抢焦点、最小化/隐藏联动、退出自动回收、z 序通过；鼠标穿透命中与渲染像素未闭环 |
| 抖音匿名链路（第四轮首次打通） | 房间页解析、FLV 取流（8s 约 11.7 MB）、弹幕 WS 握手、聊天接收（25s 136 条）通过；下播/不存在房间分类正常 |
| 中转长时与并发 | 25 分钟合成 8 路断流风暴 soak（RSS +1.28 MB、句柄恒定、0 错误）+ 8 分钟真实 huya soak（601 MB、0 错误）通过；修复 Windows 计时器粒度导致的吞吐缺陷（约 15 → 约 95 tags/s） |
| 许可收集（第四轮审计） | 实际依赖 207 个 crate、缺失许可原文 0；重导出与仓库逐文件 376=376、0 差异；字体与上游快照哈希一致 |
| 匿名手工联调（第二轮记录） | B站 / 虎牙 / 斗鱼取流与真实标题、三平台真实弹幕、真实直播流经本机 FLV 中转可读出有效 FLV |

## 4. 未验证 / 已知限制（不要当成已可用）

- **真实播放器窗口仍未实机验收**：未打开真实 mpv / PotPlayer 视频窗口，未验证画面、全屏与窗口标题；
  播放器跟随只用合成窗口验证。覆盖层鼠标穿透只有样式位证据，WindowFromPoint 与弹幕/SC 渲染像素未闭环。
- **抖音**：匿名解析 / 取流 / 弹幕已实测通过；登录态（扫码 / 网页登录）、账号核验与登录专属内容未验证。
- **登录态**：四平台凭据导入与作用域已接入，但真实登录后取流 / 弹幕 / 账号核验仍需实测。
- **长时稳定性**：只覆盖 25 分钟合成 + 8 分钟真实流；中转的会话累计重连上限（约 1–1.7 小时）待产品决策，
  跨小时 / 24 小时与 32 位时间戳回绕未测。
- 富表情 / 图片弹幕、礼物与进场事件建模、原版全部窗口行为与设置项、AI 搜索、完整重播处理均未实现。
- 外部播放器 mpv / PotPlayer / yt-dlp 由用户自行提供，本预览不打包、不代管其许可义务。
- 发布打包（最终预览重建与对外发布）不在本文件完成范围内；本文件随预览目录分发，用于说明当前状态。

## 5. 第三方组件与许可

- 本工程保留原 C# 工程的 `LICENSE` 原文，不另行变更整套代码的授权声明；Cargo package 设置为不可发布。
- Rust 运行依赖锁定在 `Cargo.lock`。构建交付前由 `scripts/Export-Licenses.ps1` 从实际依赖图收集
  原始许可文件到 `licenses/rust/`，并生成带 SHA-256 的 `licenses/rust/SOURCES.json`；
  上游发布包不带许可原文的 crate，其原文与固定 revision 快照记录在 `licenses/upstream/SOURCES.json`。
  预览目录同时携带 `licenses/`、`Cargo.lock` 与 `THIRD_PARTY_NOTICES.md`。
- 平台协议与算法参考（Streamlink / blivedm / douyinLive / 虎牙协议研究参考等）的署名、快照 revision
  与许可原文位置见 `THIRD_PARTY_NOTICES.md` 和 `licenses/SOURCES.json`。
- 中文字体为未修改的 HarmonyOS Sans SC Regular（Copyright 2021 Huawei Device Co., Ltd.），
  许可原文见 `assets/fonts/LICENSE.txt`；字体不独立再分发、不修改。
- 预览目录不含 mpv / PotPlayer / yt-dlp / WebView2 Runtime 或原 C# 应用文件。

## 6. 数据与隐私边界

- 默认数据目录为 `%LOCALAPPDATA%\LiveHubRust`，`--data-dir` 可指定独立目录；自检始终使用临时目录。
- 不读取原 C# 应用的收藏、历史、Cookie、浏览器 profile 或运行日志；不扫描浏览器 Cookie 数据库。
- 凭据只接受显式导入的文本文件，按平台隔离并以 Windows DPAPI 加密；不写入日志或 URL；
  子进程只接收凭据目录路径，不接收明文 Cookie。
- 预览包 `manifest.json` 只包含版本、阶段、目标平台、源码状态与包内相对路径 / 大小 / SHA-256，
  不含本机绝对路径、用户名、主机名或工具链信息；`scripts/Test-Preview.ps1` 会校验文件清单、哈希、
  确定性排序、路径越界、符号链接/联接点、凭据类文件名以及包内已知本机路径字符串。

## 7. 运行与卸载

- 绿色预览：解压后直接运行 `live-hub-rust.exe`（GUI 子系统，双击不弹控制台）。
- 命令行用法见包内 `README.md`；离线自检：`live-hub-rust.exe self-test`。
- 卸载：关闭程序后删除预览目录；用户数据保存在 `%LOCALAPPDATA%\LiveHubRust`，需要时单独删除。

## 8. 维护者发布检查清单

```powershell
# 1) 离线测试（隔离工具链；TargetDirectory 必须是纯 ASCII 路径）
pwsh -NoProfile -File scripts/Test.ps1 -ToolchainRoot '<isolated-rust-root>' -TargetDirectory '<ascii-build-dir>'

# 2) 生成预览：默认会在非 ASCII 仓库路径下明确报错，需要显式传 ASCII TargetDirectory；
#    生成后自动调用 Test-Preview 校验
pwsh -NoProfile -File scripts/New-Preview.ps1 -ToolchainRoot '<isolated-rust-root>' -TargetDirectory '<ascii-build-dir>'

# 3) 手工复核（可选）
pwsh -NoProfile -File scripts/Test-Preview.ps1 -Path artifacts/preview -ToolchainRoot '<isolated-rust-root>'
```

- 发布前核对 `Cargo.lock` 未变、`licenses/rust/SOURCES.json` 由实际依赖重新生成、`THIRD_PARTY_NOTICES.md`
  与实际依赖一致；预览包因而不含任何本机路径。
- 本工程不 commit / push / 发布；正式对外发布需另行审查。
