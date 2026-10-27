# 上游 issue 草稿（direction-5，2026-10-28）

流程铁律：**我起草 → 用户过目 → 说发才发**。本目录三份草稿均未发送。

| 草稿 | 目标仓库/入口 | 提交方式 | 用户动作 |
| --- | --- | --- | --- |
| [draft-1-gfxstream-device-lost.md](draft-1-gfxstream-device-lost.md) | github.com/google/gfxstream（issue） | `gh` 已登录 Acfufu，说发即发 | 过目 + 点头 |
| [draft-2-issuetracker-vid-readback.md](draft-2-issuetracker-vid-readback.md) | Google Issue Tracker（Android emulator 组件） | 用户浏览器提交（需 Google 账号） | 过目 + 复制粘贴提交 |
| [draft-3-maaend-vid-probe.md](draft-3-maaend-vid-probe.md) | github.com/MaaEnd/MaaEnd（已定位，4.2k★，C++，v2 分支）；建议抄送 MaaXYZ/MaaFramework | `gh` 或网页 | 过目 + 点头 |

证据源（本机）：
- `~/Library/Logs/DiagnosticReports/qemu-system-aarch64-2026-10-2*.ips`（崩溃栈，见各草稿附录）
- `~/Library/Logs/Google/AndroidStudio*/idea.log`（mvk-error / device lost 原话）
- `~/Library/Logs/azurelane-watchdog/`（被动取证报告，含时间线）
- 模拟器版本：`emulator --version` → 37.2.12.0 (build_id 16428233)

提交前 checklist（每份通用）：
1. 机器信息脱敏确认（草稿只用 M-series/AGX，无序列号）。
2. 崩溃栈只贴关键帧 + 说明可附完整 .ips。
3. 复现步骤是否愿意公开（均为无账号/API key 的本机操作）。
