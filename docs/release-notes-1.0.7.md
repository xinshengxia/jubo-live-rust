# 1.0.7（2026-11-07）

修复 mpv 长时间播放音画不同步（声音持续提前），把修复固化到默认启动参数，并为发布包补齐 OpenAL 配置资源。

## 根因与修复

- 现场诊断：mpv 直接使用自带 WASAPI 音频输出时，把约 576 帧（约 12 毫秒）的待播放队列报告成约 5.59 秒设备延迟。mpv 按错误的设备延迟调度音频，听感上声音持续提前，而内部 avsync 仍接近 0。
- 默认 mpv 参数增加 `--ao=openal`；本程序拉起的 mpv 子进程设置 `ALSOFT_DRIVERS=wasapi` 与 `ALSOFT_CONF` 指向包内 `assets/audio/openal-wasapi.ini`（`[wasapi]` `exclusive-mode=false`、`spatial-api=false`），改用 OpenAL Soft 的 WASAPI 缓冲/延迟模型。
- 启动前做一次无窗口 `--ao=help` 能力检查；所选 mpv 不支持 OpenAL 时返回可操作错误，不静默无声播放；用户高级参数中显式写 `--ao=`/`--audio-device=` 仍可覆盖默认。
- 保留 `--autosync=30`、`--video-sync=audio`、`--framedrop=vo`；新增 `--stop-playback-on-init-failure=yes`。策略只作用于 mpv 子进程，不写系统环境变量或全局 mpv 配置。
- PotPlayer 启动路径不变：PotPlayer 不使用 mpv 的音频后端，其同步仍由自身渲染器/设置决定；本版不修改其配置。

## 验证

- 当前实播会话切换 OpenAL 后用户确认同步与音质正常；随后连续两小时实播复测（10:58–12:58，7198 个样本）未复现累计漂移：音频预读量首尾五分钟中位数差约 2.5 毫秒，mpv 音频输出警告、缓冲暂停、解码丢帧均为 0，中继上游重连/时间线跳变均为 0。
- 离线全量测试：**746 passed / 0 failed / 31 ignored**（47 个常规测试目标 + doc-tests，48 条 test result）；相对 1.0.6 基线 730/0/30 为 +16 passed、+1 ignored，全部为本次新增测试（mpv_audio 单元测试、计划契约测试与 1 个 ignored 真实探测测试），无既有测试被跳过或改为忽略。`cargo check --all-targets --offline --locked` 退出 0，改动文件无新增警告。
- 发布包 lite ZIP 通过 manifest/许可/隐私门禁与解包自检；source ZIP 与标签提交一致。详细记录见本次发布说明。

## 已知边界

- 本机只有一种声卡/驱动组合可复现该延迟误报；不能推断所有设备都会出现，也不能证明 OpenAL 对所有设备都更优。
- 未重新做多平台真实长时直播验收；用户实际听感仍以现场为准。
- PotPlayer 未做改动；其自身长时同步本次未取得独立证据。
