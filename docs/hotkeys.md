# 全局热键（src/hotkeys.rs）

> 任务：task-12。实现位置：`src/hotkeys.rs`（纯逻辑 + Windows 注册封装）、`tests/hotkeys_contract.rs`。
> 类型定义在 `src/core.rs`（`HotkeyBindings`，由 overlay-impl 维护）；本模块在同一 crate 内为其实现解析/校验/注册 API。
> 对应原版：`danmaku-1-config.cs` 的 11 个 `Hotkey*` 字段 + `danmaku-4-overlay.cs` 的 `RegisterHotkeys/TryHotkey/WndProc(WM_HOTKEY)`。

## 1. 默认键位（与原版一致）

| 动作 | core 字段 | 默认键 | C# 常量 |
| --- | --- | --- | --- |
| 开关弹幕 | toggle_danmaku | Ctrl+Alt+D | HotkeyToggleDanmaku（overlay.cs:288） |
| 切换 SC 形态 | cycle_sc | Ctrl+Alt+S | HotkeyCycleSc |
| 隐藏/显示覆盖层 | toggle_overlay | Ctrl+Alt+O | HotkeyToggleOverlay |
| 字号变大 | font_up | Ctrl+Alt+Up | HotkeyFontUp |
| 字号变小 | font_down | Ctrl+Alt+Down | HotkeyFontDown |
| 显示区域变大 | area_up | Ctrl+Alt+Right | HotkeyAreaUp |
| 显示区域变小 | area_down | Ctrl+Alt+Left | HotkeyAreaDown |
| 弹幕变快 | speed_up | Ctrl+Alt+PgUp | HotkeySpeedUp |
| 弹幕变慢 | speed_down | Ctrl+Alt+PgDn | HotkeySpeedDown |
| 延迟减少 | delay_less | Ctrl+Alt+[ | HotkeyDelayLess |
| 延迟增加 | delay_more | Ctrl+Alt+] | HotkeyDelayMore |

固定键（不可配置，用户要求保留提示行；由 overlay 在**播放器前台时**用 RegisterHotKey 注册、离开前台注销，注册失败退回 GetAsyncKeyState 轮询）：F7 字号变小、F8 字号变大、F9 弹幕层、F10 SC 层 —— 映射见 `fixed_overlay_action(vk)`。
空字符串 = 关闭该条绑定（兼容原版手改 danmaku.json 的习惯）。

## 2. 解析规则（parse_spec）

- 形式：`修饰键+…+按键`，大小写不敏感、顺序任意、修饰键最多各一次；例：`ctrl + alt + d`、`ALT+Control+D`。
- 修饰键：Ctrl/Control、Alt、Shift、Win/Windows/Meta/Super。NoRepeat 可写但无实际差异（注册时总是加 MOD_NOREPEAT，避免按住连发）。
- 按键：A-Z、0-9、F1-F24、Up/Down/Left/Right（可写 ArrowUp…）、PgUp/PgDn/PageUp/PageDown、Home/End/Insert/Ins/Delete/Del/Backspace/Tab/Enter/Return/Escape/Esc/Space，以及单字符 ; = , - . / \ [ ] ' 反引号（OEM 虚拟键）。
- 输出：format_hotkey 规范化成 Ctrl+Alt+Down 这种形式（修饰键顺序 Ctrl、Alt、Shift、Win；按键名按上表规范化）；normalize_spec = parse + format。
- 错误类型：HotkeyParseError::{Empty, TrailingSeparator, UnknownToken, DuplicateModifier, MissingKey, MultipleKeys}，均带中文 Display，设置页可直接展示。

## 3. 绑定表校验（HotkeyBindings::parse_all，实现在 hotkeys.rs）

- 跳过空字符串；其余逐条 parse。
- 重复组合 → HotkeyConfigError::Duplicate { first, second, spec }。
- 裸 F7-F10（无修饰键）→ HotkeyConfigError::Reserved（与覆盖层固定键冲突；加任意修饰键即可）。reserved_conflicts() 用于设置页提示而不阻断保存。
- HotkeyBindings::spec(action) / set_spec(action, spec) 供设置页读写单项。

## 4. Windows 注册封装（GlobalHotkeys）

- 线程约束：RegisterHotKey(hwnd = null) 把 WM_HOTKEY 投递到调用线程的消息队列；apply 与 poll 必须在同一线程（overlay 的 follow 线程）。debug 构建有断言。
- apply(&HotkeyBindings) -> Vec<HotkeyRegisterError>：先清空旧注册；逐条注册（id = HOTKEY_ID_BASE + action.index()，0x4C00 起，应用可用区间）；单条失败只记录错误继续；配置级错误（解析/重复/保留键）整表拒绝并返回一条错误。不 panic。
- poll() -> Vec<HotkeyAction>：PeekMessageW(WM_HOTKEY, PM_REMOVE) 循环；非阻塞；无注册时安全。
- is_registered / registered_count / errors / clear；Drop 自动 UnregisterHotKey（成对清理）。
- action_for_hotkey_id(id) 供「overlay 窗口 subclass 捕 WM_HOTKEY」的替代接线路径（message == 0x0312 时调它翻译）。
- 非 Windows：stub 返回「全局热键仅支持 Windows」错误，保证库可跨平台编译。
- 失败降级：热键被其他程序占用时只丢失那一条；overlay 应把 errors() 写日志并在设置页提示；其余热键继续工作。

## 5. overlay-impl 接线清单（≤10 行，overlay.rs）

1. use crate::hotkeys::{GlobalHotkeys, HotkeyAction};（use 区）
2. 在 follow 线程（现有 native::follow 的 Hotkeys 附近）创建 let mut global = GlobalHotkeys::new();
3. 启动时：for err in global.apply(&options.hotkeys) { diag_log(...); }（仅当 options.behavior.hotkeys_enabled；关时 global.clear()）
4. 每 tick（现有 hotkeys.update(...) 位置）：for action in global.poll() { send_hotkey(action_to_overlay(action)); } —— 可映射到现有 OverlayHotkey 通道（字号±/弹幕开关/SC 开关已有）或新增分支（开关覆盖层/区域±/速度±/延迟±）
5. 设置变更（热重载/mtime 轮询）后重跑第 3 步的 apply（内部会先 clear）
6. 进程退出：Drop 自动反注册；follow 线程提前结束时显式 global.clear()
7. 固定 F7-F10 保留现状（现有 Hotkeys 裸键 + PeekMessageW），可用 fixed_overlay_action(vk) 统一映射
8. 设置页保存前调用 options.hotkeys.parse_all()，把 Display 错误显示在控件旁（ui-impl）
9. 日志：注册失败行带 action label + spec + 错误信息（HotkeyRegisterError 已实现 Display）
10. 不改变现有热键行为语义（弹幕开关/SC/字号仍即时保存并 ShowTransient 反馈）

## 6. 用户验收步骤（真实全局热键，人工）

1. scripts/Test.ps1 默认矩阵测试全绿（纯函数 + 契约测试，不注册 OS 热键）。
2. 运行本地预览包 → 打开播放器与覆盖层 → 逐条按默认键验证：Ctrl+Alt+D 开关弹幕、Ctrl+Alt+S 切 SC 形态、Ctrl+Alt+O 隐藏/显示覆盖层、↑/↓ 字号、←/→ 显示区域、PgUp/PgDn 速度、[ / ] 延迟；每次应有状态提示。
3. 设置页改一条热键（如 Ctrl+Alt+Shift+D）保存 → 重启覆盖层（或等热更新）→ 新键生效、旧键失效；再改回。
4. 人为占用一条组合（另开程序注册同一全局热键）→ 覆盖层日志出现「注册失败」且其余热键仍可用。
5. 退出覆盖层进程 → 确认无残留全局热键（UnregisterHotKey 成对）。
6. 该测试在 Windows 真实会话执行；无桌面会话的 CI 不跑（#[ignore] 的真实注册单测仅在人工验收时运行）。

## 7. 证据与限制

- 纯函数/契约测试：cargo test --locked --offline --no-default-features hotkeys（离线，不注册真实热键）。
- 真实注册单测 manual_register_poll_unregister_roundtrip 标记 #[ignore]，需用户在 Windows 桌面会话手动运行。
- 未验证项：真实多键盘布局/中文输入法占用组合、UAC 提权进程、远程桌面会话下的 RegisterHotKey 行为 —— 留给用户验收记录。
## 8. 0.3.1 接线状态

- overlay 已通过 SharedOptions 版本化下发接入 11 个动作（注册失败逐条 diag/overlay.log 记录，F7–F10 保留）；设置页「界面与热键」tab 可编辑/校验/持久化，保存时 parse_all + overlay.validate。
- 真实按键触发仍需真实桌面验收（见 §6）。

## 9. 0.3.13：固定 F7–F10 改为「前台时吞键」

背景：0.3.12 把 F7–F10 改成 GetAsyncKeyState 轮询后，按键不再被系统消费 —— 前台 PotPlayer 同时收到 F9（打开浏览器面板）和 F7（弹出播放控制），用户实测反馈。

- `hotkeys::GlobalHotkeys::set_fixed_active(active)`：仅当播放器进程是前台时注册四个裸键（id 0x4D00..0x4D03，MOD_NOREPEAT，vk 0x76..0x79），否则注销；状态未变时不碰系统，可每 tick 调用。
- 注册期间系统吞掉 F7–F10，前台播放器（PotPlayer/mpv）收不到；离开播放器前台立即注销，不干扰其它程序。
- `poll_events()` 同时排空配置键（0x4C00 起）与固定键（0x4D00 起）的 WM_HOTKEY，overlay 的 follow 线程统一分发到 `OverlayHotkey` 通道。
- 兜底：`fixed_registered(action)` 为 false 的键（被其它程序占用/注册失败）仍由 overlay 的 GetAsyncKeyState 按下沿轮询触发，并在 overlay.log 记录失败原因。
- 线程约束与配置键相同：RegisterHotKey(NULL) 与 PeekMessageW 必须同线程（overlay follow 线程）。
- 验收：真实播放器前台按 F7/F8/F9/F10，播放器不应再响应（PotPlayer 浏览器面板/播放控制不出现），覆盖层字号/弹幕/SC 状态正常变化；真实桌面项见 §6。
