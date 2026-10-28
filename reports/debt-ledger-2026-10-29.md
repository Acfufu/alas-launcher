# ALAS 技术债台账（2026-10-29 夜间事故后建账）

来源：`log/error/1793206933866`（IslandProduction 撞岛屿加速券对话框 → GamePageUnknownError → exit(1)）排查与修复。
当晚已修（payload 26fa148cf → 2850b9684，全部已 push 自建仓 + rsync bundle）：加速券对话框逃生、asset 整屏截图约定修正、资源不足 toast→驳回、recipe counters=None 防御（+缩进回归修复）、NetworkDown/购买成功/养成计划三条逃生规则。
本台账只记**未修**的债。证据文件均指 `/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript/log/`。

## P1-1 夜间游戏客户端卡顿（底层放大器）

- **现象**：2026-10-29 00:56–01:02 岛屿订单页 UI 响应 21–38 秒（正常 2 秒），引发当晚连锁 GameStuck/连点超限；04:30 客户端掉线弹 [NetworkDown]。
- **证据**：`2026-10-28_alas.txt` L113300 附近（点击→需求页 21.6s/38s）；NetworkDown 报错 `error/1793219457407`；adb offline 波动 `2026-10-29_alas.txt` 04:28 段（注意：该次系排查连拍打挂 adb，属自伤，别归因）。
- **假设**（待证）：模拟器用户态网络栈（slirp）间歇停顿 / guest DNS / 宿主 Wi-Fi 省电 / 游戏频道 SDK 后台行为。
- **推进步骤**：
  1. logcat 按 NetworkDown 与卡顿时间窗切片：`adb -s emulator-5554 logcat -t '<time>'`，找 netd/connectivity/DNS/GC 风暴/system_server 卡顿。
  2. 宿主侧：Wi-Fi 省电、VPN/代理、`log show` 对应时段。
  3. 部署 60s 一次的 `adb shell ping <游戏服务器>` RTT 记录（`probe/` 下加脚本；**严禁**高频 screencap——见 P1-0 教训）。
  4. 关联记忆 [[guest-watchdog-soft-reboot-20261028]]（guest 僵死家族），对比签名。
- **验收**：定位到一层（网络栈/客户端/宿主）并有数据支撑；若为环境问题给出规避（如保活脚本），不写代码修复。

## P1-2 IslandOrder 需求计数 OCR 误读审计

- **现象**：`error/1793220305166` 截图显示订单要求胡萝卜 0/14（红字），`is_order_satisfied` 却判满足 → 进 submit_order → 游戏 toast「资源不够啦」拒绝（已兜底驳回）。
- **对象**：`module/island/order.py` `REQUIREMENTS_COUNTER_OCR`（IslandReversedDigitCounter，红字 sub_letter 逻辑）。
- **推进步骤**：用 `error/1793220305166` 等截图对 counter OCR 跑正负样本，确认误读模式（红 0/14 被读成什么）；修 OCR 解析或加「红字不足→直接判不满足」的颜色门。
- **验收**：对全部当晚截图，扫描结果与人工判读一致。

## P2-1 COMMISSION_DAILY 连点超限——复发再诊

- 04:30:57 那次真凶是 [NetworkDown] 弹窗（`error/1793219457407`），弹窗逃生规则已上线。**先观察**：若无 NetworkDown 的情况下再次出现 `Too many click for a button: COMMISSION_DAILY`，再诊断委托模块本身（`error/1793219457407/log.txt` 有完整点击史）。

## P2-2 养成计划（CULTIVATING PLAN）要不要做成正式 task

- 现状只有逃生规则（popups_main `cultivating_plan`，回 BACK_ARROW_WHITE）。若该 2026-10 活动值得自动化（探索者养成），需要 page 定义 + 导航 + 完整流程——按方向类立项，不走债。

## P1-0 工装与流程教训（已入 memory，防复发）

- **asset PNG 必须整张 1280x720 截图**（area 载入时裁）——存小图=全黑模板=恒 1.0 万能命中。见 memory `alas-asset-png-full-screenshot-convention`。
- **验证 harness 必须两侧都走 `load_image`（PIL/RGB）**：`cv2.imread`（BGR）对彩色模板（如蓝色 NetworkDown 头部）有 ~0.15 通道惩罚，会把正样本误判为 0.848<0.85。灰度系模板（加速券头部）碰巧免疫，别侥幸。
- **adb 流量纪律**：严禁高频 screencap 连拍（60s 一次的 ping/事件级单拍可接受）——04:28 的 adb offline 是自己打的。
- **popups_main 新规则清单**（截至 2850b9684）：network_down(置顶)、island_speed_up、island_buy_success、cultivating_plan。后续新弹窗照此模式：裁全屏 asset → load_image 双侧正负样本验证 → 入组 → commit+push+rsync。
