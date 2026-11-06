# 1.0.6（2026-11-07）

本轮按 C# 1.1.6 生产链对照补齐「某些抖音房间 Rust 打不开、C# 能打开」的完整恢复链，并收尾原报告明确未实现的抖音富事件与 C# B站 Brotli ver3。构建与测试结果见文末；未在本机真实平台验证的项目单独列明。

## 抖音起播恢复链

1. **业务结果驱动的换路**：房间页请求不再只在连接报错时换路。HTTP 200 验证码页、空壳页、格式变化页都会继续尝试下一连接路径（pinned IPv4 → 系统解析 → 系统代理），最多 3 条路径，整体共享一个有界 deadline；明确下播/房间状态异常立即返回，不反复换路。全部路径失败时按「验证码 > 限流 > 格式 > 网络」给出类型化错误。
2. **Windows 系统代理回退（只读）**：读取 HKCU Internet Settings 的 ProxyEnable/ProxyServer/ProxyOverride 构建代理路径，支持 host:port、http=…;https=… 与 bypass 列表；代理不可用/被 bypass 时跳过该路径且不计网络失败。**不修改用户代理设置**；Clash TUN 模式（fake-IP 198.18.0.0/15）继续由 pinned/系统解析路径直接处理，两套模式都保留。
3. **验证恢复闭环**：解析遇到验证要求时，用本应用专属 profile 的隐藏浏览器静默刷新一次 Cookie（single-flight + 60 秒冷却 + 可取消），成功只重试当前房间一次；仍失败给出「请在账号与登录页完成抖音网页验证」的可操作错误，不循环、不重复开播放器。中继进程遇到同样错误会进入 verification 等待态：保持播放器连接、不消耗重连预算、不在冷却期反复重发解析。
4. **用户正常验证入口**：账号页抖音卡片新增「抖音网页验证」可见窗口（本程序专属 profile + CDP 捕获，用户自己完成滑块/点选），捕获到 ttwid 且页面不再是验证码页后加密保存并失效页面缓存；取消即关闭自有窗口，不读取日常浏览器登录态。
5. **CDN/协议兼容**：媒体地址不再要求固定 CDN 家族——语法/凭据/私网/本机名称策略保留，未知公网主机解析后必须为公网或 TUN fake-IP；中继逐跳仍做有界 DNS、公网过滤与地址钉扎。初始媒体地址保留原协议（http 不再强制升级），跳转允许 http→https/同协议、拒绝 https→http 降级；跨主机跳转不转发 Origin、Referer 仅在目标同初始主机或已知 CDN 家族时保留，Cookie/Authorization 永不进入媒体请求。
6. **同房间请求合并**：状态/标题/头像查询使用同房间单飞 + 4 秒短复用（凭据指纹 + 凭据写入/验证成功时主动失效），起播始终读取新页面，不能复用陈旧媒体直链；多个用途并发不再放大抖音房间页请求。
7. **解析容错**：FLV 画质键按大小写不敏感匹配（优先级不变）；验证码判定收窄为「验证码中间页 / sec_sdk_build / 小于 20 KiB 且含 captcha」，正常房间页里的 captcha 脚本名不会误判；HTTP 401/403 与 200 验证码页都会把系统代理提到下次请求的首位。

## 抖音富事件

- WebcastGiftMessage → 礼物事件（礼物名 + repeat_count，回退 combo_count）、WebcastMemberMessage → 进场、WebcastLikeMessage → 点赞统计行，映射到既有 DanmakuEvent::Live 契约，不扩展共享事件类型。字段布局按两个独立 MIT 公开实现（lulajax/DouyinLiveJava、jwwsjlm/douyinlive-proto）逐项交叉核实，登记见 THIRD_PARTY_NOTICES.md 与 licenses/SOURCES.json；解析上限复用现有实现，坏 payload 只跳过该条。
- **边界**：只做协议定义可证的部分，本机未做真实平台联调；关注/分享/人数/大表情聊天/电商等仍跳过。抖音没有房间级表情接口（两版都只有平台级 emoji/list，不猜）；虎牙 WUP 动态表情包未迁移（需实机抓包，不在不猜协议的前提下实现）；动画表情 C#/Rust 都不渲染，不宣称支持。

## 构建与验证

- 改动应用版本：Rust 1.0.6；C# 1.1.7。
- Rust 全量离线测试 **729 passed / 0 failed / 29 ignored**（48 targets）；release 包 467 条目、manifest 一致、私人路径 0 命中，
  `--version` 1.0.6、离线自检 7/7；`artifacts/current` 已切换，上一版 release-1.0.5（自检 7/7）保留。
- C# 从干净 HEAD `fe1990c` 一次 headless 构建（`1.1.7+fe1990cd5777`），离线测试 **228 passed / 0 failed / 12 skipped**、
  5815 断言；Brotli ver3 合成向量 4 用例/13 断言通过；回退集 `artifacts/rollback-1.1.6`。
- **真实平台验收（匿名/TUN，只读）**：真实抖音直播间取流与中继成功（http 直链保留，5s 158 帧/1.6MB）；
  真实验证码窗口返回类型化错误；全新专属 profile 的静默刷新按设计识别验证码并返回 false（不保存伪 Cookie，
  25s 内关闭，profile ACL 已收紧为 OWNER+SYSTEM）；真实抖音弹幕收到进场 383/458/140 与点赞 33/44/2，
  Enter/Like 适配已实机验证，礼物事件约 8 分钟内未出现、仍仅协议+合成验证。
- C# 1.1.7 线上 ver3：36 个真实 brotli 帧经生产解码路径 43/43 帧成功、解出 10 条 DANMU_MSG（证据在本机验收记录）。
- 仍未验证：已通过验证的持久 profile 上的静默刷新成功、真实风控页的人工滑块交互、系统代理模式（未改用户设置）、
  真实礼物事件、物理声画与长时多窗口。详见 docs/validation.md。
