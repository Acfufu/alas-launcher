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

## P1-2 IslandOrder 需求计数 OCR 误读审计 ✅ 已结（2026-10-29 追认审计）

- **现象**：`error/1793220305166` 截图显示订单要求胡萝卜 0/14（红字），`is_order_satisfied` 却判满足 → 进 submit_order → 游戏 toast「资源不够啦」拒绝（已兜底驳回）。
- **对象**：`module/island/order.py` `REQUIREMENTS_COUNTER_OCR`（IslandReversedDigitCounter，红字 sub_letter 逻辑）。
- **审计结论（原假设被证据推翻）**：counter OCR 从未误读红字。当晚 log 里 6 处红字行全部读对（(0,24,-24)/(0,9,-9)/(0,14,-14)/(0,17,-17)/(0,1,-1)/(0,3,-3)）；`bitwise_and(main, sub)` 在"黑=0"灰度编码下实为**并集**（0&x=0），红 0 与深灰 /14 合并成完整 0/14，实现本身正确。两张事故截图走真实 OCR 路径（PIL/RGB）复扫全部与人工判读一致。
- **真凶（两起 ISLAND_ORDER_ACCEPT 连点崩溃 04:41:46→04:42:02、04:44:46→04:45:05）**：①运行实例当时还没有 toast→驳回逃生（a28d54c4a 04:48 才提交，事故在其前）——当晚已闭环；②**面板内容滞后渲染（P1-1 卡顿家族）**：04:41:46 扫描读到白菜/大豆/大米全满足、04:42:02 截图却是实用之木 0/9；04:44:46 读到大豆/鲜肉+空行、04:45:05 截图已是胡萝卜 0/14——扫描与提交之间面板被换过，OCR 忠实读的是残影。
- **顺带修掉的结构隐患**：真实行被遮挡/滞后读成 (0,0,0) 时旧代码 `id is not None and counter is not None` 把它当「需求 0」收进 requirements → 判满足。
- **已上线（payload d94e85ba9 + 86a9b2478，已 push 自建仓 + rsync bundle）**：
  1. `scan_current_order_requirements` 返回 `OrderRequirementScan(requirements, readable, red_insufficient)`：counter 区有墨无数字 / 有数字无名 / 整板全空 → readable=False 绝不提交；execute_order 等 1s 复扫一次，仍不可读走不满足路径（赛季单不清 stuck id，防错误 replan）。
  2. 红字颜色门：counter 区红像素 >30 → red_insufficient，无视数字直接不满足（即本条原建议方案，留作 OCR 退化保险）。
  3. 空行判定用 **counter 区墨水量**（实测：真实行 87–374px、空行恰 0、红行 red 65 vs 非红 0，门限 30 两端成立）；名字区墨水被 toast 暗条污染（空行也能量出 476px）不可作判据。
- **验收**：当晚全部带需求面板的截图（04:42:02、04:45:05；其余 error dir 经查看均非需求面板）新逻辑复扫 = NOT satisfied 与人工判读一致；全黑/全白合成图 fail-safe（readable=False）；ruff 零告警。工具：`dev_tools/audit_island_counter_ocr.py`（真实 cached_property 网格 + object.__new__ 跳过 init）。
- **残余风险（挂 P1-1 名下观察）**：面板内容滞后 >1s 时复扫追不上，错误提交=被拒→toast 逃生驳回→10min 冷却，有界不崩溃；等 P1-1 网络栈定位后复查。

## P2-1 COMMISSION_DAILY 连点超限——复发再诊

- 04:30:57 那次真凶是 [NetworkDown] 弹窗（`error/1793219457407`），弹窗逃生规则已上线。**先观察**：若无 NetworkDown 的情况下再次出现 `Too many click for a button: COMMISSION_DAILY`，再诊断委托模块本身（`error/1793219457407/log.txt` 有完整点击史）。

## P2-2 养成计划（CULTIVATING PLAN）要不要做成正式 task

- 现状只有逃生规则（popups_main `cultivating_plan`，回 BACK_ARROW_WHITE）。若该 2026-10 活动值得自动化（探索者养成），需要 page 定义 + 导航 + 完整流程——按方向类立项，不走债。

## P1-0 工装与流程教训（已入 memory，防复发）

- **asset PNG 必须整张 1280x720 截图**（area 载入时裁）——存小图=全黑模板=恒 1.0 万能命中。见 memory `alas-asset-png-full-screenshot-convention`。
- **验证 harness 必须两侧都走 `load_image`（PIL/RGB）**：`cv2.imread`（BGR）对彩色模板（如蓝色 NetworkDown 头部）有 ~0.15 通道惩罚，会把正样本误判为 0.848<0.85。灰度系模板（加速券头部）碰巧免疫，别侥幸。
- **adb 流量纪律**：严禁高频 screencap 连拍（60s 一次的 ping/事件级单拍可接受）——04:28 的 adb offline 是自己打的。
- **popups_main 新规则清单**（截至 2850b9684）：network_down(置顶)、island_speed_up、island_buy_success、cultivating_plan。后续新弹窗照此模式：裁全屏 asset → load_image 双侧正负样本验证 → 入组 → commit+push+rsync。
