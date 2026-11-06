# 第三方来源与许可

本项目是 live-hub 的独立 Rust 重构工程。保留原工程的 LICENSE 原文；本次重构不另行作出整套代码的许可变更或授权声明。Cargo package 设置为不可发布。

## Rust 运行依赖

使用 Cargo.lock 锁定版本与 registry 校验值。构建前核对上游声明，构建交付时由 scripts/Export-Licenses.ps1 从实际 Cargo 依赖图收集原始许可文件；收集目录与机器路径分离，公开清单只写 crate 名、版本、来源、commit（如有）、原相对路径与 SHA-256。尚未完成许可收集验收的构建不得视为可对外发布包。

主要直接依赖为 anyhow、base64、chrono、clap、eframe/egui、flate2、fs2、image、md5、qrcode、reqwest、serde、serde_json、tempfile、tungstenite、url、uuid，以及 Windows 目标下的 windows-sys。具体采用版本及完整传递依赖以 Cargo.lock 和收集清单为准，不以本文的概括替代其原文。

## 平台协议与算法

- **Streamlink**：虎牙 anti-code / 斗鱼取流签名流程参考及适配。上游为 [streamlink/streamlink](https://github.com/streamlink/streamlink)，核对快照 `0045aae912972122b8a9c81cc4a0056d93b986c2`，BSD-2-Clause；版权 Christopher Rosell 与 Streamlink Team。见 licenses/streamlink-LICENSE.txt，适配范围为 src/platforms.rs。未携带 Python 运行时或 Streamlink 程序。
- **blivedm**：B站弹幕协议与消息模型参考，未引入 Python 程序或绘图引擎。上游 [xfgryujk/blivedm](https://github.com/xfgryujk/blivedm)，原工程核对快照 `3bf17fe862b8c2fc6bc04c81c3f0c5db95de93d6`；MIT，Copyright (c) 2018 xfgryujk。原文见 licenses/blivedm-LICENSE.txt，使用范围为原生协议客户端及相关测试。
- **抖音弹幕签名与协议**：纯算法 `frontierSign`/`X-Bogus` 签名、PushFrame、protobuf 字段布局与 WebSocket 生命周期
  由本仓库旧 C# 实现迁移；其中 DouyinSign 适配自 **今晚打打** 的上游实现（MIT，Copyright (c) 2025 今晚打打），
  许可原文见 `licenses/douyinLive-LICENSE.txt`。真实平台联调未包含在离线测试中。
- **抖音富事件协议定义**：抖音直播间 WebcastGiftMessage / WebcastMemberMessage / WebcastLikeMessage
  的 protobuf 字段布局（message、字段名、编号、类型）与事件路由语义参考两个独立维护的 MIT 公开实现；
  只采用协议定义与字段语义，未复制上游源码、未引入其运行时或第三方签名服务：
  - [lulajax/DouyinLiveJava](https://github.com/lulajax/DouyinLiveJava)，MIT，Copyright (c) 2026
    DouyinLiveJava contributors，核对快照 `b812242787ae9b5f4bbbf4e2239e4eec43318119`；依据
    `client/src/main/proto/douyin.proto`（SHA-256 `3ecf4474c9608aa048bc438a039d0210d7a9292bc5c936df5de280178cb2f1c0`）
    与 `client/src/main/java/com/douyinlive/mapper/MessageRouter.java`；许可原文见
    `licenses/DouyinLiveJava-LICENSE.txt`。
  - [jwwsjlm/douyinlive-proto](https://github.com/jwwsjlm/douyinlive-proto)，MIT，Copyright (c) 2026 jwwsjlm，
    核对快照 `0d661be57c8f80947054b2ef0cd1ac2fdce3cc4b`；依据 `protobuf/new_douyin.proto`
    （SHA-256 `d00e9d8dd59df4fc8925172d2a9d5fa9a129a96ee567bdff2e4a9e74deefa2ef`）；许可原文见
    `licenses/douyinlive-proto-LICENSE.txt`。两来源在实现使用的字段编号上逐项一致；本轮只用合成
    protobuf 向量验证，未做真实平台联调，不能据此声称富事件已在真实房间验收。
- **弹幕富媒体与 mpv 脚本移植**：表情代码字典、图片表情头尺寸校验、emoji 清单解析/分段、SC/勋章/等级等消息字段
  移植自本仓库旧 C# 实现（同一作者，同一 GPL-3.0 项目家族）；其中 emoji 区段为 Unicode 标准数据。彩色 emoji 精灵图
  assets/emoji-sheet.png/.txt 由旧 C# 工程在构建期用系统字体渲染生成，随预览包分发；如对字体渲染物的再分发有疑虑，
  可在打包前删除这两个文件（运行时自动降级为单色），或自行提供 LIVE_HUB_EMOJI_SHEET 覆盖。assets/lua/live-title.lua、assets/lua/live-resync.lua
  同样移植自旧 C# 工程 lua/ 目录，随预览包分发并只在 mpv 播放路径加载。
- **虎牙弹幕协议**：Tars/JCE 子集、匿名 WebSocket 订阅与心跳、聊天字段解析按本仓库旧 C# 实现的实测记录编写，
  并用公开协议研究参考交叉核对（hwenjie/huya_danmu、BacooTang/huya-danmu、biliup-start/VideoPlayer）。
  仅参考消息号与收发路径，未复制第三方代码，故不附额外许可原文。Tars 为腾讯协议格式（Tencent/Tars，BSD-3-Clause），
  本工程只实现观察到的线上子集。
- **虎牙、斗鱼表情映射**：虎牙默认 39 项及已知补充/覆盖项、斗鱼默认 250 项由旧 C# 同项目实现迁移；名称与 CDN URL 来源于各平台官方网页。源码仅保留映射，平台图片在运行时按需加载，不随包捆绑或另行授权。
- FLV framing 和时间线处理按媒体格式实现，使用合成测试夹具，不复制真实直播内容。

## 许可收集与发布边界

`licenses/rust/SOURCES.json` 由 `scripts/Export-Licenses.ps1` 按 Cargo.lock 的实际依赖图生成并记录哈希；
其中 13 个 crate 的上游发布包不自带许可原文，对应原文与固定 revision 快照记录在
`licenses/upstream/SOURCES.json`（仅许可文本，未引入第三方代码）。构建/打包对外发布前仍必须：
核对 Cargo.lock 未变、重新生成清单、检查最终包内容和本文件，不能仅凭本仓库已有清单视为已通过发布验收。

2026-10-29 的许可审计（隔离工具链实际运行 `scripts/Export-Licenses.ps1`）：Cargo.lock 解析出的
207 个实际依赖均已收集到许可原文，缺失 0；`licenses/rust/SOURCES.json` 与重新导出的清单逐文件哈希一致；
13 个上游快照 crate 的 9 个许可文件哈希、字体与 3 个平台算法许可原文哈希均与清单一致。
预览包清单与内容规则由 `scripts/Test-Preview.ps1` 校验（含确定性排序、路径越界、链接与凭据文件名）。

## 字体

0.2.0新增二维码生成和PNG/JPEG/WebP解码依赖；2026-10-30重新从实际Cargo依赖图收集209个依赖，缺失许可原文0。具体逐文件哈希保存在`licenses/rust/SOURCES.json`。图片内容仅运行时按允许的平台CDN加载，不捆绑或重新授权平台头像、表情和直播内容。

本软件使用 **HarmonyOS Sans Fonts**，Copyright 2021 Huawei Device Co., Ltd.。随程序嵌入未修改的 HarmonyOS Sans SC Regular，许可原文见 assets/fonts/LICENSE.txt。字体不得独立再分发或修改；该许可声明不扩大到其他第三方素材。

## 外部程序

mpv、PotPlayer 和 yt-dlp 由用户另行提供。本工程的源码及预览构建不打包这些程序，使用时各自适用原许可。尤其不能将支持 PotPlayer 误认为获得其再分发权。新工程也不携带原应用的凭据、配置、收藏、历史或浏览器 profile。
