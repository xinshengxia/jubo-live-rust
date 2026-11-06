# 1.0.1

日期：2026-11-01。基线：1.0.0（`artifacts/release-1.0.0`）。

## 用户可见变化

- 抖音直播弹幕里的行内表情码（`[捂脸]`、`[微笑]`、`[赞]` 等）现在显示为平台表情图片，不再显示成方括号文字；与 B站 图片表情共用同一条「字典 → 文本分段 → 有界远程图片」渲染路径。
- 换房间/换平台后覆盖层会刷新表情字典（同一覆盖层不再只认第一次拿到的字典）。

## 根因

Rust 版移植时只接了 B站 房间表情字典（`GetEmoticons`）的后台拉取；抖音 `emoji/list` 的解析函数虽然已经写好，但没有任何生产调用。覆盖层也只在启动后第一次拿到非空字典时读一次缓存，因此抖音房间的字典始终为空，`[xxx]` 全部按原始文本绘制。

这与 WebP 解码器无关：真实抖音表情是 **VP8L 无损 WebP 96×96**，项目的 `image` 0.25.10 + `image-webp` 0.2.4 能正常解码（证据见 `docs/validation.md` 的 1.0.1 小节）。

## 修复

- `src/danmaku_douyin.rs`：新增 `EMOJI_LIST_URL` 与 `fetch_emoticons_result`。浏览器 User-Agent + `Referer: https://www.douyin.com/` + JSON Accept；导入 douyin 域凭据时按域携带 Cookie；1 MiB 响应上限、HTTP 状态/UTF-8/JSON/空字典分别给出可诊断错误。
- `src/danmaku.rs`：新增 `fetch_douyin_emoticons(_result)`、`fetch_douyin_emoticons_anonymous(_result)`；连接抖音时后台线程拉取一次并写入 `EMOTICON_CACHE`（空字典不覆盖旧字典、取消后不写入）；新增 `EMOTICON_CACHE_VERSION` 缓存代次与 `cached_emoticons_version()`。
- `src/danmaku_rich.rs`：抖音字典解析改为遍历 `emoji_url.url_list` 取首个通过白名单的 URL（之前只看第一项），跳过空 `display_name`。
- `src/overlay.rs`：字典按缓存代次刷新（覆盖「首次到达 + 换房/换平台替换」两种场景）；本地 `LIVE_HUB_EMOTICON_JSON` 同时接受 抖音 `emoji_list` 与 B站 `GetEmoticons` 两种响应形状。
- 版本、能力清单、对照清单、抖音协议文档与本发布说明同步为 1.0.1。

## 验证

- 全量离线测试（GUI 与 `--no-default-features`）：见 `docs/validation.md` 1.0.1 小节。
- 真实抖音匿名 `emoji/list` 验收：真实公网返回 214 条，含 `[捂脸]`/`[微笑]`；生产解码路径把真实 VP8L 表情解成 96×96 RGBA；`split_text_segments` 把 `[捂脸]` 分成图片段。
- 离线渲染证据：旧版不注入字典时显示方括号文字；新版/旧版注入真实字典 JSON 后渲染出表情图片（证明渲染与 WebP 解码链路本来可用，缺的是字典拉取）。

## 回退

`artifacts/release-1.0.0` 完整保留；如需回退，退出所有 Rust 程序/播放会话后把 `artifacts/current` junction 指回 `artifacts/release-1.0.0`。个人配置、账号、收藏、历史与 `.local/user-data` 不随本版本变化。
