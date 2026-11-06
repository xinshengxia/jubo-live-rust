# 0.3.2 本地迁移预览（登录浏览器隐私修复）

更新：2026-10-31。基线 0.3.1；只修复网页登录窗口的浏览器账号问题，其余行为不变；数据与 0.3.1 兼容。

## 问题（用户实拍）

虎牙扫码登录时，独立登录窗口使用 Microsoft Edge，Windows 账户（Microsoft 账号）被 Edge 隐式登录，弹出“在所有设备上同步你的浏览数据”，用户无法判断发生了什么，也不希望登录窗口使用个人账号。

## 修复

- **优先使用 Chrome 做网页登录**：Chrome 没有 Windows 账户隐式登录能力，登录窗口使用本程序新建的全新临时资料目录，不读取日常浏览器资料；若本机未安装 Chrome，才回退 Edge。
- **Edge 回退时抑制账号/同步相关特性**：--disable-sync、--disable-features=msImplicitSignin,msEdgeIdentity,msEdgeSync,msEdgeFirstRunExperience,msEdgeSidebar，并保留 --no-first-run --no-default-browser-check --new-window 与独立 --user-data-dir。
- 网页登录窗口/进度文案更新：明确“全新临时资料，不会登录或同步你的 Edge/Chrome 账号；如出现浏览器同步提示可直接关闭”。
- 登录完成后仍通过本进程专属 DevTools 端点（端口所有权校验）读取 Cookie，再用平台接口核验并 DPAPI 加密保存；不扫描日常浏览器资料。

## 验证

- cargo check --locked --offline --all-targets EXIT=0；cargo test --lib **296 passed / 0 failed / 2 ignored**（含新增 login::browser::tests::login_browser_never_signs_into_the_user_account：Chrome 参数含 disable-sync/no-first-run、Edge 回退参数含 msImplicitSignin/msEdgeSync/msEdgeFirstRunExperience）。
- 0.3.1 的完整两矩阵/UI/覆盖层证据仍然有效；本版仅登录浏览器启动参数与界面文案、版本号变化。
- 真实扫码登录/真实平台核验仍需用户验收（本机 Chrome 启动与 DevTools 端口已实测可用：Chrome/156，DevToolsActivePort 正常、无个人账号资料）。

## 未验证 / 限制

- 真实虎牙/斗鱼/抖音扫码、登录后 Cookie 捕获与平台核验需真机操作。
- 若用户机器只有 Edge，同步提示可能仍由 Edge 版本/系统策略决定；本版已关闭相关特性并明确文案，仍以实际观察为准。
- 真实桌面同步弹窗的视觉验证未自动化。
