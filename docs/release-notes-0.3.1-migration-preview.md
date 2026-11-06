# 0.3.1 本地迁移预览（用户实测修复 + 上轮遗留收口）

更新：2026-10-31。基线 0.3.0；本版只修缺陷与补齐上一轮标注的遗留项，数据目录与 0.3.0 兼容（state.json 旧字段自动兼容）。

## 修复（用户 0.3.0 实测反馈）

- **所有下拉框点不开**：workbench dropdown 旧实现用 deprecated Memory popup 且未每帧保活，egui 下一帧即按 abandoned 清除。重写为显式 open 状态（点击 toggle、每帧 keep_popup_open + Popup::from_response().open(true)、行点击/外部点击/Esc 关闭）；launch 卡 5 个下拉与弹幕设置页全部生效，并加 headless 帧测试（打开→保持→选择→重开→外部关闭）。
- **收藏卡/会话行头像只显示首字**：B站 getInfoByRoom 本机返回 -352 风控，旧解析恒失败。改为 room_init(canonical+uid) → get_anchor_in_room.face → Master/info → getInfoByRoom 回退；增加失败分类诊断，重试窗口 20→10 分钟。真实网络实测 bili:7777 取到 96px 头像并落盘 48px PNG + index.json；虎牙/斗鱼可下载，抖音风控时如实分类（UI 回退首字）。
- **右上角圆钮图标偏小**：圆 20 / 内部 glyph 10 / 描边 1.6 / 间距 34（圆心像素与原版 ≤0.2px）。
- **下拉 chevron 太丑**：细 V（stroke 1.35、8×5、右 14、hover 变亮）。
- **侧栏图标/对齐**：重画账号（头+肩弧）、观看会话改纯显示器、新增 Monitor 供会话/chrome/侧栏品牌使用；文字回到 31 基线；图标中心差 ≤0.6px、文字垂直中心 ≤1px。
- **弹幕“一个一个蹦 / 右边始终没有弹幕 / 顶部裁切”**：根因是覆盖层 update_metrics 在字体/DPI 变化路径只重排、从未同步 scheduler viewport，scheduler 永远保留构造时 640×240 —— 新弹幕从中左“蹦出”、右半屏永无入场弹幕、纵向旧高导致贴顶裁切。新增 LaneScheduler::set_viewport + 每帧防御校验后修复；resize 仍保 progress。真实房间“稀疏”另有原因（房间发言限制+合并窗口，实测 8s 2–6 条、浏览器对照 3–4 条/5s），已加 15s 无消息状态说明，不虚增速率。

## 补齐（上一轮“未完成”）

- **11 个可配置全局热键**：设置页「界面与热键」tab 可编辑/校验（格式、重复、裸 F7–F10 冲突）/恢复默认/持久化；覆盖层接线 11 动作，F7–F10 保留，注册失败降级并写日志。
- **窗口可见性策略**：默认“播放器可见即显示”（AbovePlayer），AlwaysOnTop→Topmost，HideWhenNotForeground 仅 topmost 时生效；window_policy 决策接线并有合成场景证据。
- **SC P1**：badge 高度≈57/右缘对齐；compact 测量截断+省略号+×间距；scale200 不越界；face_frame 头像框；message_trans 译文（full 形态正文下）。
- **事件卡**：左上状态块（350×ui、行高 30×ui、背景、每秒限流、礼物合并、参与弹幕避让）。
- **固定弹幕避让**：由“整垂直带屏蔽”改为“居中行与 SC 卡片矩形相交才避让”，resize 后重算居中。
- **合并弹幕 ×N 放大**、**滚动条拖动**（纯函数映射+单测；真实鼠标未自动化）、**规则**（re:/uid:/name:/关键词：屏蔽/高亮/置顶）、**覆盖层日志**（连接失败/规则错误/热键注册失败/策略隐藏/30s 统计，有界脱敏）。
- **界面字体/字号设置**：自动/系统雅黑/内置鸿蒙 + 85/100/115/130，重启生效（含重启启动器按钮）。
- **保存/导入/导出**：收藏页「导入收藏/导出收藏」（导出 JSON 可被导入回读，往返测试 7/7；原生打开/保存对话框）；数据页「导入原版数据…/导入原版历史…」（兼容旧版 C# 收藏数组/结构化导出/历史文档）。
- **搜索候选详情字段**：直播状态/平台更新/命中依据/核验说明/核验时间/来源（缺失不编造），UI 详情与来源面板已展示。
- **URL 语法**：支持 m.douyu.com、www.douyin.com、live.bilibili.com/blanc|h5/<id> 等原版可识别格式。
- **崩溃/中转日志**：panic hook 写 <数据目录>/logs/panic.log（64 KiB 有界，保留最新）；relay 连接/重连/退出等写 diag 日志。
- **窄窗适配**：页面内容裁切到 host、收藏工具栏/提示与 data 头部在 760×520 下换行不压页脚。

## 验证

- 最终两矩阵（冻结树，2026-10-31）：默认 features **587 passed / 0 failed / 17 ignored**（EXIT=0；2026-10-31 勘误：早前误报 541/0/12，同日志重算为 587/0/17）；--no-default-features **393 passed / 0 failed / 16 ignored**（EXIT=0）；树 hash 前后一致。新增契约/定向测试（dropdown 交互、favorites_transfer+e2e、avatars 真实网络 ignored 2/2、rules/diag/sc_fields/lookup_fields/relay_log/hotkeys/window_policy、UI 字体 130% A/B）全部通过，详见 [验证报告](validation.md) §0.3.1。
- 覆盖层右缘修复、窗口策略、SC P1、事件卡等合成证据与未验证边界见本机验收目录 notes/r2-overlay-evidence.md；本发布包只含离线可复现结果。

## 未完成 / 未验证（下一轮）

- **彩色 emoji 精灵图 PLAIN 区段偏移**：PNG 与清单由同一生成脚本可复现，但 1F300 区段格子内容相对清单偏 6 格（该问题在原版资产中已存在）；本轮交付契约测试与调查，资产替换/生成链定位留下一轮。
- 真实全局热键按键、真实鼠标拖动滚动条、真实桌面下拉点击、原生导入/导出对话框、多显示器/独占全屏、真实付费 SC、真实平台登录：需真实桌面验收。
- 浅色主题 placeholder 对比度等细节。