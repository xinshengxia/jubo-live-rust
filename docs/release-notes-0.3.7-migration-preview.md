# 0.3.7 本地迁移预览（文字堆叠 / 快捷键重试 / 离线网页房间号 / 头像索引）

更新：2026-10-31。基线 0.3.6；数据与旧版兼容。

## 用户反馈与修复

1. **候选/详情文字堆叠**：平台简介里的换行符被单行卡片绘制成多行并溢出到下一行；候选行与详情“简介”现在先做空白归一化，详情只显示单段。
2. **全局快捷键仍无效**：注册全局热键前先用 PeekMessage 预建线程消息队列；注册失败每 2 秒重试并写 overlay.log（global hotkey retry:），便于区分“注册被占用/失败”与“动作未分发”。F7–F10 仍只在播放器前台注册，Ctrl+Alt+* 常驻。
3. **抖音账号解析失败**：verify 现在在拿不到“正在直播场次”时按原版思路兜底：user_info.web_rid → 账号主页 roomStore/web_rid → 候选自身数字房间号；命中即绑定 room=web_rid、live_status=offline、标注“核验时未开播，可直接收藏”。live 账号仍走公开 reflow（owner.web_rid）。
4. **收藏头像不全（抖音/虎牙别名首字）**：
   - avatars 服务新增持久化 remembered URL：lookup 候选选中、核验成功、加入收藏前都会 remember(platform, room, avatar_url)（HTTPS + 平台 CDN 白名单），重启后收藏卡优先用 remembered URL 下载。
   - 虎牙别名房（a16789/clkimmy 等）的头像解析新增官方别名页 → 数字 profileRoom → 平台头像 API 的兜底。
   - 已把原版 C# runtime/workbench-avatars.json 的 34 条 URL/PNG 迁移进 Rust 数据目录（备份 index.json.before-csharp-migration.bak），现有收藏卡可直接显示真实头像。

## 验证

- cargo check --all-targets EXIT=0；cargo test --lib **305 passed / 0 failed / 2 ignored**。
- 真实网络：抖音逐候选 verify 不再出现跨域 Cookie；离线账号当前公开页面没有 roomStore/web_rid 时仍会明确提示“未返回直播场次或数字网页房间号”（不会伪造房间）。
- 真实 Huya 别名头像：a16789、clkimmy 均解析到 huyaimg.msstatic.com 头像。
- 收藏头像迁移：离屏 ui-shot 已确认 B站收藏卡显示真实头像；抖音/虎牙切换 tab 后应同样显示（C# 索引 34 条全部迁移）。
- 两矩阵/包复核见 docs/validation.md 与 notes/verification-r7.md（verifier 填）。

## 限制

- 抖音离线账号若公开页面不提供 web_rid/roomStore，仍无法绑定房间（原版同样依赖直播 reflow）；可等开播后重新解析或使用直播分享链接。
- 快捷键需真实播放器前台场景复测；仍无效时看 overlay.log 的 global hotkey 行。
