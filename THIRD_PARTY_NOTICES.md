# 第三方来源与许可

本项目是 [jubo-live](https://github.com/xinshengxia/jubo-live) 的 Rust 重构，保留原工程 GPL-3.0 的 LICENSE 原文。项目许可不取代第三方组件各自的许可。

## Rust 依赖

版本与 registry 校验值以 Cargo.lock 为准；`licenses/rust/SOURCES.json` 记录实际依赖的原始许可文件与 SHA-256。缺少随 crate 发布的许可原文时，固定上游快照见 `licenses/upstream/SOURCES.json`。依赖许可原文均随源码与程序包携带。

## 协议与算法

- **Streamlink**：虎牙 anti-code / 斗鱼取流签名流程参考及适配。上游为 [streamlink/streamlink](https://github.com/streamlink/streamlink)，核对快照 `0045aae912972122b8a9c81cc4a0056d93b986c2`，BSD-2-Clause；版权 Christopher Rosell 与 Streamlink Team。见 licenses/streamlink-LICENSE.txt，适配范围为 src/platforms.rs。未携带 Python 运行时或 Streamlink 程序。
- **blivedm**：B站弹幕协议与消息模型参考，未引入 Python 程序或绘图引擎。上游 [xfgryujk/blivedm](https://github.com/xfgryujk/blivedm)，原工程核对快照 `3bf17fe862b8c2fc6bc04c81c3f0c5db95de93d6`；MIT，Copyright (c) 2018 xfgryujk。原文见 licenses/blivedm-LICENSE.txt，使用范围为原生协议客户端及相关测试。
- **抖音弹幕签名与协议**：纯算法 `frontierSign`/`X-Bogus` 签名、PushFrame、protobuf 字段布局与 WebSocket 生命周期
  由本仓库旧 C# 实现迁移；其中 DouyinSign 适配自 **今晚打打** 的上游实现（MIT，Copyright (c) 2025 今晚打打），
  许可原文见 `licenses/douyinLive-LICENSE.txt`。真实平台联调未包含在离线测试中。
- **弹幕富媒体与 mpv 脚本**：相关消息字段、布局语义和 Lua 脚本沿用同一 GPL-3.0 项目家族的 C# 实现。Unicode 区段数据用于表情索引。公开发行图集使用 Noto Emoji 字体重新生成，不携带 Windows 系统字体或其逐字形位图转换物。
- **虎牙弹幕协议**：Tars/JCE 子集、匿名 WebSocket 订阅与心跳、聊天字段解析按本仓库旧 C# 实现的实测记录编写，
  并用公开协议研究参考交叉核对（hwenjie/huya_danmu、BacooTang/huya-danmu、biliup-start/VideoPlayer）。
  仅参考消息号与收发路径，未复制第三方代码，故不附额外许可原文。Tars 为腾讯协议格式（Tencent/Tars，BSD-3-Clause），
  本工程只实现观察到的线上子集。
- FLV framing 和时间线处理按媒体格式实现，使用合成测试夹具，不复制真实直播内容。

## 字体和表情

- **HarmonyOS Sans SC Regular**：Copyright 2021 Huawei Device Co., Ltd.，字体原文许可在 `assets/fonts/LICENSE.txt`，字体文件按原许可未修改。不得把此许可扩大到其他素材。
- **Noto Emoji**：Copyright 2013 Google LLC，SIL Open Font License 1.1。固定上游提交和字体 SHA-256 见 `assets/emoji-source.json`，原始许可在 `licenses/NotoEmoji-LICENSE.txt`。`assets/emoji-sheet.png` 由该字体渲染并按图集索引排布；`scripts/generate_emoji_sheet.py` 可重建。
- 平台头像、在线表情和直播媒体仅按需加载，不在发布包内，版权归原权利人。

## 外部播放器

本次 lite 程序包不包含 mpv、PotPlayer 或 yt-dlp。用户自行提供程序，各自适用原许可；支持调用 PotPlayer 不代表拥有其再分发权。
