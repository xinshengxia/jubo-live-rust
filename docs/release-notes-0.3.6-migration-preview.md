# 0.3.6 本地迁移预览（查找页 7 项缺陷修复）

更新：2026-10-31。基线 0.3.5；数据与旧版兼容。

## 用户反馈的查找页问题与修复

1. **DeepSeek 设置面板文字堆叠/与候选重叠**：AI 设置改为独立固定高度面板（320px）并按内容排版，新增「收起设置」；不再压到候选列表。
2. **候选卡文字挤压**：候选行 52→64px，名称·平台 / 房间·粉丝·简介 / 身份·核验状态三行布局，每行按宽度省略号截断，超长昵称/简介不再溢出压行。
3. **排序不一致**：单平台搜索结果与单平台缓存命中统一调用共享 `rank_candidates`（全部平台 combine 原本已排序），同分线索按平台无关的同一套规则。
4. **选中不自动核验**：单击候选即自动 `start_verification`（带搜索代次/防重复），双击与「重新解析房间」保留；未选中时详情文案改为“已返回候选：左侧选择候选后自动核验”。
5. **抖音核验报 “Refusing to send cookies outside the selected platform”**：抖音账号资料（`www.iesdouyin.com`）与直播场次 reflow（`webcast.amemv.com`）改走**无 Cookie 的公开请求**（固定 HTTPS host 白名单、不带任何凭据），不再触发跨域 Cookie 守卫；需登录 Cookie 的请求保持原守卫。
6. **收藏选中橙黄框不完整**：选中描边改为内容之后最后绘制，并外扩 1px，四边不再被封面渐变/内部按钮覆盖。
7. **候选头像不显示**：4 平台搜索候选本来已解析 `avatar_url`（B站 hdslb / 虎牙 msstatic / 斗鱼 douyucdn / 抖音 douyinpic）；本轮补齐候选列表纹理加载：有房间候选复用平台头像服务（磁盘缓存+去重），仅账号候选用其公开 avatar URL 后台下载（白名单+256KiB+超时，失败保留首字占位，单 URL 去重/上限 64）。

## 验证

- `cargo check --all-targets` EXIT=0；`cargo test --lib` 301 passed / 0 failed / 2 ignored；`lookup_fields_contract` 6 passed；`avatars_contract` 13 passed。
- 真实登录态：抖音搜索后逐个核验候选，不再出现跨域 Cookie 错误（当前无人在播，返回“账号资料已核实，但未返回直播场次”）。
- 离屏 ui-shot（2480×1440）：候选三行布局、DeepSeek 面板、收藏选中描边见 notes/evidence-r7/。
- 两矩阵与最终包复核见 docs/validation.md 及 notes/verification-r7.md（verifier 填）。

## 限制

- 真实 Edge InPrivate 扫码登录全流程、真实直播场次 reflow、真实平台头像 CDN 在用户网络下的显示仍需实机确认。
- 抖音接口若再次调整风控，可能仍返回空；可用 search.log / 网页辅助排查。
