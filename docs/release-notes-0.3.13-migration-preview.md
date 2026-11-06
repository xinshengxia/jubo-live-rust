# 0.3.13 本地迁移预览（覆盖层穿透/固定键/SC 滚轮/主界面圆角）

更新：2026-11-01。基线 0.3.12；数据与旧版兼容（可直接回退 0.3.12，无数据迁移）。

## 修复（用户实测反馈）

### 1. 有 SC 时覆盖层偶发挡住鼠标，PotPlayer 全屏底部控制栏不出现

- 根因：主覆盖层用 `WM_NCHITTEST` 对非 SC 区域返回 `HTTRANSPARENT` 做「选择性穿透」。但 `HTTRANSPARENT` 只会向**同一线程**的下层窗口继续转发；PotPlayer 在另一个进程，系统不会把鼠标继续交给播放器。因此只要出现过 SC 卡片（窗口被当成可交互），整片画面就可能吃掉鼠标：播放器收不到 hover/移动，全屏底部控制栏不出现，用户观感就是「有时候不穿透」。
- 修复：
  - 主覆盖层窗口**永久** `WS_EX_TRANSPARENT`，不再参与鼠标命中；
  - 新增独立的 alpha=0 分层输入窗口，用 `SetWindowRgn` 把窗口形状精确裁成 SC 卡片 + 关闭按钮 + 滚动条矩形。区域之外的像素系统直接跳过该窗口，跨进程可靠地落到播放器；
  - 鼠标在卡片上移动时把 `WM_MOUSEMOVE` 转发给播放器，全屏控制栏仍能出现；
  - SC 命中矩形裁剪到可见带（滚动时半出屏幕的卡片不再占住屏外一整块）。
- 离线合成运行证据（fixture）：主覆盖层 `ExStyle=0x80801B0`（含 `WS_EX_TRANSPARENT`）；输入窗 `ExStyle=0x8080080`、alpha=0、区域只覆盖右上 SC 卡片区，20×20 命中图仅卡片区命中。见本机验收目录 `output/smoke-sc-input.txt`。

### 2. F9 同时打开 PotPlayer 浏览器面板、F7 同时弹出播放控制

- 根因：0.3.12 把固定 F7–F10 改成 `GetAsyncKeyState` 轮询，只「看」不「吞」，前台 PotPlayer 照样收到这些键。
- 修复：仅当播放器在前台时用 `RegisterHotKey` 动态注册 F7–F10（系统吞掉，播放器收不到），离开前台立即注销（不影响其它程序）；个别键被占用注册失败时自动回退到原轮询并在日志记录。F10 现象不变。

### 3. SC 溢出时滚轮太慢

- 根因：旧版 C# 收到的是原始滚轮 delta（120/格），`scroll -= delta * 0.6` 即 72px/格；Rust 改成 notches（delta/120）后系数没换算，一格只滚 0.6pt。
- 修复：`SC_WHEEL_STEP_POINTS = 72`，一格恢复旧版手感。

### 4. 主界面四角圆角

- 新增 `src/gui/window_frame.rs`：`DwmSetWindowAttribute(DWMWA_WINDOW_CORNER_PREFERENCE = 33, DWMWCP_ROUND = 2)`，**动态加载 dwmapi.dll**（GNU 工具链没有 dwmapi 导入库，且 Windows 10 不支持该属性，必须可失败回退）；用 `EnumThreadWindows(GetCurrentThreadId())` + 精确窗口标题取本进程 UI 线程的 HWND（不会串到另一个实例）；设置结果与 readback 写诊断日志，任何失败只 no-op，窗口保持直角。
- 最大化/贴边由 DWM 自动保持直角；Win10/非 Windows 不做任何事。

## 预览包

- `artifacts/preview-0.3.13`：437 文件，exe 21,195,821 B，sha256 `1c48f058d6e0a9f5457864037ab5599e19bc5cf42d013fc8a4bbfa48c78924c0`。
- Test-Preview：FilesChecked 439 / Entries 437 / ManifestMatches true / SortedManifest true / PrivatePathFindings 0 / EXIT=0；包内 `--version` = 0.3.13，`self-test` 7/7 passed。
- `artifacts/current` 已切到 preview-0.3.13；preview-0.3.12 保留回退。

## 验证

- `cargo check --locked --offline --all-targets` EXIT=0；
- 默认 GUI 测试矩阵：**614 passed / 0 failed / 25 ignored**（42 个 test target，EXIT=0）；
- `--no-default-features` 矩阵：**416 passed / 0 failed / 24 ignored**（42 个 target，EXIT=0）；
- `cargo test --lib overlay` 53 passed；`cargo test --no-default-features hotkeys`：lib 12 passed / 2 ignored + hotkeys_contract 13 passed，0 failed。
- 合成运行证据：本机验收目录 `output/smoke-sc-input.txt`（覆盖层/输入窗样式与区域命中图）。
- 仍需用户真实验收：真实弹幕下的 SC 点击/滚轮/滚动条拖动、PotPlayer 全屏底部控制栏、F7/F8/F9/F10 吞键、主界面圆角观感。

## 限制

- 真实窗口、真实鼠标与真实键盘行为无法离线证明；真实付费 SC 与真实平台登录仍是用户验收项。