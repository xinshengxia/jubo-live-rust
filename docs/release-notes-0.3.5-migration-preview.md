# 0.3.5 本地迁移预览（Edge 隐私登录 + 抖音搜索收敛处理）

更新：2026-10-31。基线 0.3.4；数据与旧版兼容。

## 用户反馈与修复

### 1) 网页登录恢复使用 Edge，但不登录/同步 Microsoft 账号

- 老版 C# 使用内嵌 WebView2（Edge 内核）登录，所以从不会出现外部 Edge 的账号同步提示。
- 0.3.2 为绕开 Edge 隐式登录改用了外部 Chrome；这偏离了老版体验。0.3.5 改为：优先 Edge，以 InPrivate 隐私窗口启动，并使用独立 `--user-data-dir` + `--disable-sync` + 关闭 `msImplicitSignin/msEdgeSync/msEdgeIdentity/msEdgeFirstRunExperience`；无 Edge 时回退 Chrome 独立临时资料。
- 登录窗口不读取日常浏览器资料，不会登录或同步 Microsoft/Google 账号；Cookie 仍由本进程专属 DevTools 端点读取、平台接口核验、DPAPI 加密保存。

### 2) 抖音“查找主播”为 0 的收敛处理

- 0.3.4 已按老版补齐官方搜索接口的完整查询参数与浏览器请求头（真实登录态实测“陈泽/花小龙”各 10 个候选）。
- 0.3.5 进一步处理偶发空结果：空列表不再写入查询缓存、空/失败自动重试一次、搜索平台结果写入 `<数据目录>/logs/search.log`（只记平台/数量/错误，不记 Cookie）；状态栏对“抖音成功但空结果”给出明确提示。

## 其它

- 用户原版 C# 收藏（34 条）已用 CLI 导入本机 Rust 数据目录（33 新增 + 1 更新；含 huya/douyu/douyin 房间）；重启 0.3.5 后收藏页可见。导入引擎与 GUI「导入原版数据…」使用同一 `Store::import_json`。

## 验证

- cargo check --all-targets EXIT=0；cargo test --lib 300 passed / 0 failed / 2 ignored。
- 真实验证：Douyin 搜索使用本机凭据库（不打印 Cookie）返回 10 个候选（0.3.4 已测，0.3.5 请求代码不变+重试/缓存处理）。
- 最终两矩阵与包校验见 docs/validation.md（待 verifier 填入）。

## 未验证 / 限制

- 真实 Edge InPrivate 窗口的扫码登录全流程需用户实机确认（本机 Edge/Chrome 均安装）。
- 抖音接口若再次调整指纹/签名要求，可能仍返回空；可看 search.log 或使用“抖音网页辅助”。
