# 0.3.12 本地迁移预览（B站 CDN 302 跳转放行，PotPlayer 标题后缀恢复）

更新：2026-10-31。基线 0.3.11；数据与旧版兼容。

## 修复

- 用户用 0.3.11 的 PotPlayer 打开 B站仍无标题参数。日志显示中转首个连接立即 upstream policy rejected：B站直播 FLV 会 302 跳到腾讯 CDN https://<hash>.v.smtcdns.net/d1--cn-gotcha04.bilivideo.com/...，此前媒体 CDN 白名单只有 bilivideo.com，把这条跳转拒绝了。
- 0.3.12 将 smtcdns.net 加入受信媒体 CDN 白名单（仍要求 HTTPS、公网 DNS、固定地址、重定向次数限制；路径仍指向 bilivideo 对象），并补了 B站 smtcdns 白名单测试。
- 修复后本机 0.3.12 源码实测 B站 7777 PotPlayer 标题：老实憨厚的唐 - 德云色 热游畅玩...  ·  1920x1080 60fps h264 8836kbps。
- 0.3.11 的抖音主播前缀、虎牙别名房标题、PotPlayer relay→直连探测兜底全部保留。

## 额外修复：F7/F8/F9/F10 固定快捷键

- 用户反馈左下角 F7/F8 字号、F9 弹幕、F10 SC 提示存在但按键无反应。旧实现用 RegisterHotKey 注册这四个裸 F 键，PotPlayer 或其它前台程序占用组合时会静默失败。
- 0.3.12 改为在中转覆盖层的 50ms 跟随循环里用 GetAsyncKeyState 轮询按键状态：只有播放器在前台时，检测到按下沿才触发对应动作。不再依赖全局热键注册，也不会被播放器抢走按键。

## 验证

- cargo check --all-targets EXIT=0；cargo test --lib 310 passed / 0 failed / 2 ignored（新增 B站 smtcdns 测试）。
- 真实：B站 7777 PotPlayer 标题带完整参数；其他三平台标题路径不变。
- 两矩阵/包复核见 docs/validation.md 与 notes/verification-r12.md（verifier 填）。

## 限制

- 若 B站再换新的 302 CDN 家族，需要把具体 host 加入白名单并补测试；playback/relay 日志会保留失败原因。
