# 覆盖层窗口可见性 / 层叠策略（src/window_policy.rs）

> 任务：task-12。实现：`src/window_policy.rs`、`tests/window_policy_contract.rs`。对应原版 `danmaku-4-overlay.cs` 的 Frame() 可见性判断、EnsureZOrder()、IsOverlayWanted()、PlayerFollowPolicy。

## 1. 规则（与原版一致）

输入：WindowPolicy { always_on_top, hide_when_not_foreground }（来自 OverlaySettings）、PlayerWindowState { process_alive, window_exists, window_visible, minimized, foreground_player, foreground_overlay }、Option<ClientArea>。

| 条件（从上到下短路） | 结果 |
| --- | --- |
| 播放器进程已退出 | 隐藏（ProcessExited） |
| 播放器窗口不存在/不可见 | 隐藏（WindowMissing） |
| 播放器最小化 | 隐藏（Minimized） |
| client 不存在 / 宽或高 < 16 / > 16384 / 坐标溢出 | 隐藏（InvalidClientArea） |
| AlwaysOnTop=false（默认） | 显示，AbovePlayer（不做 TOPMOST；前台无关——其他窗口盖住播放器时自然一起被盖住） |
| AlwaysOnTop=true 且 HideWhenNotForeground=true 且前台既不是播放器也不是我们 | 隐藏（NotForeground） |
| AlwaysOnTop=true（其余情况） | 显示，Topmost |

补充：
- 「前台是我们」= 覆盖层自身窗口或其托盘菜单在前台（原版 IsOverlayWanted 同样放行 foreground == Handle）。
- 负坐标虚拟屏（副屏在左）可用；ClientArea::sanitized() 拒绝溢出 i32 的矩形。
- 退出规则 should_exit_after_missing_window(process_alive, missing_ms)：进程确认死亡或窗口缺失 ≥30000ms → 退出；进程查询失败（None）本身不作为退出证据，但也不会抑制窗口缺失超时（原版 PlayerFollowPolicy: processAlive == false || missingMs >= WindowGraceMs）。

## 2. API

- WindowPolicy::from(&OverlaySettings)（默认 always_on_top=false / hide_when_not_foreground=true）。
- decide(policy, &player, area) -> OverlayWindowDecision { visible, topmost, z_order, hide_reason, area }；ZOrderAction::{None, AbovePlayer, Topmost}。
- ClientArea::{is_usable, sanitized, origin, size}、MIN_CLIENT_SIDE=16、MAX_CLIENT_SIDE=16384。
- 纯函数、无 Windows/egui 依赖 → 离线测试覆盖真值表。

## 3. overlay-impl 接线清单（overlay.rs follow 线程）

1. use crate::window_policy::{ClientArea, PlayerWindowState, WindowPolicy, ZOrderAction, decide};
2. 每 tick 组装状态：process_alive（现有 PlayerProcess::alive()）、window_exists/visible（IsWindow/IsWindowVisible）、minimized（IsIconic）、foreground_player（现有 GetForegroundWindow 判 pid，含旧 4 层父窗口遍历）、foreground_overlay（前台 == overlay hwnd）
3. let decision = decide(WindowPolicy::from(&options), &state, area.map(Into::into));
4. decision.visible 决定 ShowWindowAsync（隐藏 SW_HIDE）；隐藏原因写日志（限频）
5. ZOrderAction::Topmost → SetWindowPos(HWND_TOPMOST, SWP_NOMOVE|SWP_NOSIZE|SWP_NOACTIVATE)；AbovePlayer → 沿用现有 GetWindow(target, GW_HWNDPREV) + 插到其后的逻辑；None → 不动
6. 位置/尺寸继续用 decision.area（client rect）；替换现有 should_show(...) 调用
7. F7-F10 的「仅播放器前台时注册」逻辑保持不变（固定键，不是可见性）

## 4. 与原实现/原版的差异

- Rust 旧 should_show 要求 foreground_player —— 会导致副屏/分屏看直播时弹幕消失；本策略按原版默认语义改为「播放器可见即显示」。
- 原版每帧 SetWindowPos 只在层叠需要时调用（GWL 检查）；本策略把「要不要动层叠」交给 overlay 的现有避免重复调用逻辑，不影响正确性。
- 未验证：多显示器混合 DPI、全屏独占（exclusive fullscreen）下的前台判定、覆盖层被其他 topmost 程序遮挡 —— 需真实窗口验收。

## 5. 离线测试

cargo test --locked --offline --no-default-features window_policy：
默认模式不依赖前台、topmost 模式 + HideWhenNotForeground 的真值表、我们自己的窗口在前台不隐藏、最小化/窗口消失/进程退出/非法 client 区域的隐藏原因、负坐标虚拟屏、30s 退出宽限、非法尺寸拒绝。
## 6. 0.3.1 接线状态

- overlay follow 线程已用 window_policy::decide 替换旧 should_show：默认「播放器可见即显示/AbovePlayer」，AlwaysOnTop→Topmost，HideWhenNotForeground 仅 topmost 时生效；合成真窗场景证据见本机验收记录 与 notes/r2-overlay-evidence.md。
- 多显示器/独占全屏/混合 DPI 仍待真实窗口验收（见 §4）。
