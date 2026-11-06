# 0.3.3 本地迁移预览（抖音网页登录 Cookie 修复）

更新：2026-10-31。基线 0.3.2；只修复网页登录在抖音上的 Cookie 捕获失败，其余行为不变；数据兼容。

## 问题（用户实拍）

抖音扫码登录成功，但点「我已登录，检查并保存」提示「暂未保存：Invalid cookie name。完成正常登录后可重试。」，账号页显示抖音未保存登录凭据。

## 根因

浏览器登录通过 CDP 读取本平台 Cookie。抖音登录后的 Cookie 集合里存在一个**空名称 Cookie**（Chrome 允许保留；抖音广告/SSO 页面会留下），我们的 CookieRecord 校验要求名称非空，于是整批捕获被拒绝——一个异常 Cookie 让正常会话也无法保存。

## 修复

- decode_browser_cookies 改为逐条校验：不可用的 Cookie（空名称/非法字符/非法域/值/路径）跳过并写 diag 日志（名称有界、控制字符转义、不记录值），其余 Cookie 继续保存；健康检查仍由平台接口/浏览器内 fetch 判定。
- 仅当没有任何可用 Cookie 时报明确错误「浏览器返回的Cookie均不可用（已忽略 N 个格式异常项）」。
- 账号页说明文字合并去重（修复上一版两行文字与登录面板重叠）。
- 新增单测：decode_skips_malformed_cookies_and_keeps_the_session、decode_reports_when_every_cookie_is_unusable。

## 验证

- cargo check --locked --offline --all-targets EXIT=0；cargo test --lib **298 passed / 0 failed / 2 ignored**（新增 2 个单测）。
- 真实现场诊断：用户登录窗口的 Cookie 集合 68 个中 66 个名称合法、1 个为空名称（www.douyin.com），与根因一致。
- 最终两矩阵与发布包校验见下方（待 verifier 填入）。

## 未验证 / 限制

- 真实抖音扫码登录保存后的取流/弹幕需用户实机复核。
- 其他平台若出现格式异常 Cookie 也会被跳过（属于容错，不改变正常会话）。
