# ALAS 技术债台账（2026-10-29 夜间事故后建账）

来源：`log/error/1793206933866`（IslandProduction 撞岛屿加速券对话框 → GamePageUnknownError → exit(1)）排查与修复。
当晚已修（payload 26fa148cf → 2850b9684，全部已 push 自建仓 + rsync bundle）：加速券对话框逃生、asset 整屏截图约定修正、资源不足 toast→驳回、recipe counters=None 防御（+缩进回归修复）、NetworkDown/购买成功/养成计划三条逃生规则。
本台账只记**未修**的债。证据文件均指 `/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript/log/`。
2026-10-29 收口：P1-2/P2-1/P2-2/P1-0 全部结清（文末一行账）；上午审计新发现的 P1-3 当天修复清账（payload ec343191a）。详情均在各仓库 git 历史与 memory。

## P1-1 夜间游戏客户端卡顿（底层放大器）

- **现象**：2026-10-29 00:56–01:02 岛屿订单页 UI 响应 21–38 秒（正常 2 秒），引发当晚连锁 GameStuck/连点超限；04:30 客户端掉线弹 [NetworkDown]。
- **证据**：`2026-10-28_alas.txt` L113300 附近（点击→需求页 21.6s/38s）；NetworkDown 报错 `error/1793219457407`；adb offline 波动 `2026-10-29_alas.txt` 04:28 段（注意：该次系排查连拍打挂 adb，属自伤，别归因）。
- **复发证据（2026-10-29 06:09–06:11）**：app 06:02:52 重启后的新进程（order.py 代码=1832214cb：堆栈点击行 L291 吻合；2850b9684 未动 order.py；readable 门 d94e85ba9 06:40 才提交，事故在其前）。上一单 ACCEPT 后 33s 无响应→"Confirm timer timeout, assuming submit success"（`2026-10-29_alas.txt` L18502）；下一单 06:10:06 扫描可读、判满足提交，游戏 66s 未受理 → ACCEPT 按钮持续可见被反复点击 11 次 → `error/1793225472197` 连点超限 exit(1)。refuse toast（~2s 寿命）在 2–9s 帧间隔采样下全程未入帧，toast 逃生无从触发。与 guest 僵死/面板滞后同家族；点击风暴的代码放大器见 P1-3。
- **假设**（待证）：模拟器用户态网络栈（slirp）间歇停顿 / guest DNS / 宿主 Wi-Fi 省电 / 游戏频道 SDK 后台行为。
- **推进步骤**：
  1. logcat 按 NetworkDown 与卡顿时间窗切片：`adb -s emulator-5554 logcat -t '<time>'`，找 netd/connectivity/DNS/GC 风暴/system_server 卡顿。
  2. 宿主侧：Wi-Fi 省电、VPN/代理、`log show` 对应时段。
  3. 部署 60s 一次的 `adb shell ping <游戏服务器>` RTT 记录（`probe/` 下加脚本；**严禁**高频 screencap——原 P1-0 教训，已迁 memory `alas-debt-ledger-2026-10-29`）。
  4. 关联记忆 [[guest-watchdog-soft-reboot-20261028]]（guest 僵死家族），对比签名。
- **验收**：定位到一层（网络栈/客户端/宿主）并有数据支撑；若为环境问题给出规避（如保活脚本），不写代码修复。

## 已清事项（2026-10-29 收口）

- **P1-3 submit_order 点击分支缺 clicked 锁存——连点风暴放大器（上午发现，当天修复）**：payload ec343191a——点击分支补 `clicked = True` 锁存，push 自建仓 + rsync bundle（L343 验证在位）。回归 harness `dev_tools/audit_island_submit_latch.py` 四场景全绿：修复前冻结帧场景 60 次点击（06:11 实崩同款），修复后单次提交恒 1 次点击、超时/assume-success/urgent-success/reject 兜底全部命中。可选加固（点击达上限转 reject 兜底）**未采纳**：reject_order 内层 loop 无超时，从模糊态调 reject 会引入新的 GameStuck 面，且锁存后点击天然封顶 1 次。生产确认：等下一次真实岛屿订单跑完后查 error/ 无新 ISLAND_ORDER_ACCEPT 风暴（事故全文见 payload 仓库 git 历史）。
- **P1-2 IslandOrder counter OCR 审计**：✅ 已结——假设被推翻（OCR 未误读红字），真凶=面板滞后渲染；加固 d94e85ba9 已上线。摘要在 memory `alas-debt-ledger-2026-10-29`，全文在 git 历史（1cae54b）。
- **P2-1 COMMISSION_DAILY 复发观察**：观察期无复发——10-29 全天仅 04:30:57 一起（=NetworkDown 弹窗，逃生已上线），结案。若未来无 NetworkDown 却复发再诊（完整点击史在 `error/1793219457407/log.txt`）。
- **P2-2 养成计划正式 task**：非债（立项类决策），移出台账，另行立项。
- **P1-0 工装与流程教训**：弹窗 recipe 与 adb/BGR 纪律迁入 memory（`alas-debt-ledger-2026-10-29`），本条撤出；全文在 git 历史（c575474）。
