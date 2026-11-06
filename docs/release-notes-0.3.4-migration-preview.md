# 0.3.4 本地迁移预览（抖音主播搜索恢复）

更新：2026-10-31。基线 0.3.3；只修复抖音“查找主播”搜索结果恒为 0，其余行为不变。

## 问题（用户实拍）

查找主播“花小龙”时，B站 16 / 虎牙 10 / 斗鱼 4，抖音始终 0，界面没有报错。

## 根因

抖音官方用户搜索接口（www.douyin.com/aweme/v1/web/discover/search/）对请求形态敏感：老版 C# 发送了完整官方搜索页参数（设备/浏览器指纹、pc_search_top_1_params、屏幕/CPU/内存等）以及 Edge 154 浏览器 UA、Accept、Accept-Language、Referer；Rust 迁移时只保留了基础参数，并使用通用 UA/仅 Referer，导致接口返回空用户列表（status_code=0, user_list=[]）——看起来就是“永远 0 个结果”。

## 修复（依照老版逻辑）

- 按老版 Url() 重建完整查询参数：from_group_id/disable_rs/need_filter_settings/list_type/pc_search_top_1_params/update_version_code/pc_client_type/pc_libra_divert/support_h265/support_dash/cpu_core_num/screen_width/height/browser_name=Edge/browser_version=154/engine/os/device_memory/platform/downlink/effective_type/round_trip_time 等。
- 抖音搜索专用请求头：DOUYIN_USER_AGENT（Chrome/154 + Edg/154）、Accept: application/json, text/plain, */*、Accept-Language: zh-CN,zh;q=0.9、Referer: https://www.douyin.com/；Cookie 仍按域/路径从凭据库作用域化发送。
- 解析沿用既有 parse_douyin_search（sec_uid 校验、主页链接、昵称/签名/头像/抖音号），无结果时仍不判定主播不存在。
- 新增 2 个离线测试（查询参数形状、搜索结果字段映射）；保留 env 门控诊断输出（LIVE_HUB_DOUYIN_SEARCH_DEBUG=1，仅在手动探测时打印状态码/列表长度/响应前缀，不记录 Cookie）。

## 验证

- 真实登录态（用户凭据库副本）实测：搜索“花小龙”返回 **10 个候选**（status_code=0, user_list=10），首个为花小龙本人主页；修复前同一路径为 0。
- 离线：cargo test --lib **300 passed / 0 failed / 2 ignored**（新增 2 测试）。
- 最终两矩阵（0.3.4 冻结树）：待 verifier 填入。

## 未验证 / 限制

- 真实界面点击检索后的候选展示/核验/收藏需用户实机复核。
- 抖音接口可能随时调整指纹/签名要求；若再次空结果，可用“抖音网页辅助”或反馈。
