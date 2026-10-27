# ALAS macOS 功耗探针（probe/）

对 ALAS payload 视觉管线（截图 → OCR → 模板匹配）做分相位功耗与吞吐基线的工具集。
首次完整运行：2026-10-27，MacBook Air M5 / Azurelane 5554 前台 / Arknight 已停 / ALAS webui 空闲。
同日追加：静态 N 模型 ANE 复验（run_p4_admin.sh）与 scrcpy 截图链路试验（结论见 baseline 文档附录）。

## 文件

- `bench_ocr.py` — OCR 微基准，复刻 payload `module/ocr/al_ocr.py` 的预处理/CTC 解码。
  `--engine cpu|coreml --mode mixed|bucket|fixed640`（mixed=原始动态宽度；bucket=W 上取整到 64 倍数；
  fixed640=全部 pad 到 640，配全静态模型用）。`--model` 可指向重导出的静态形状 ONNX。
  coreml 模式自动做与部署 CPU 模型的输出 parity 校验。
- `bench_screencap.py` — 截图链路基准。`--mode png`（部署默认 `screencap -p`）或 `raw`（跳过设备端 PNG 编码）。
- `bench_scrcpy.py` — scrcpy(H.264) 截图链路试验台：monkeypatch `ScrcpyOptions.command_v120`
  置 `control=false`（绕过 Android 16 剪贴板监听崩溃）+ 建空闲 control 占位连接（1.20 握手必需），
  其余走真 payload 栈，打印每帧延迟；失败时抽 server 流尾部辅助定位。配置改动快照并在 finally 恢复。
- `bench_template.py` — cv2.matchTemplate 基准（64px/128px 模板全屏搜索，2 线程与 payload 一致）。
- `phase_driver.sh` — 相位编排：P0 空闲 → P1 截图 → P2 OCR-CPU → P3 CoreML-mixed → P4 CoreML-bucket → P5 模板 → P6 三合一，各 60s；记录相位边界与 qemu CPU%。
- `run_baseline_admin.sh` — root 侧：powermetrics（2s×240 样本，cpu/gpu/ane_power）包住相位驱动。需 root。
- `run_p4_admin.sh` — root 侧 P4 ANE 复验：powermetrics（2s×90）包住 40s idle + 60s staticn(bucket)
  + 60s staticn_w640(fixed640)，产物写 `*2` 文件集，不动上午基线文件。需 root。
- `roi_watch.sh` — root 侧 ROI 剖析观察器：轮询 app python 进程，>20% CPU 连续两次确认后对目标
  py-spy record 120s@50Hz（非阻塞），并 dump 其余 python 栈作对照；单次录制后退出。
- `parse_power.py` — 解析 pm.txt + phases.log + bench JSON → 报告 md。已参数化：
  `--pm/--phases/--cpu-log/--meta/--bench-glob/--report/--title`，默认值即 2026-10-27 上午基线。

## 复跑

```bash
# 免 root 预验证（单基准短跑）
/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript/.venv/bin/python bench_ocr.py \
  --engine cpu --mode mixed --until-epoch $(($(date +%s)+12))

# 完整基线（约 8.5 分钟，期间会弹一次管理员密码框）
osascript -e 'do shell script "bash '"$PWD"'/run_baseline_admin.sh >> '"$PWD"'/out/admin_runner.log 2>&1" with administrator privileges'

# P4 ANE 复验（约 3 分钟，弹一次密码框；游戏须保持前台）
osascript -e 'do shell script "bash '"$PWD"'/run_p4_admin.sh >> '"$PWD"'/out/admin_p4_runner.log 2>&1" with administrator privileges'
# 解析（默认即上午基线；*2 文件集示例）
/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript/.venv/bin/python parse_power.py \
  --pm out/pm2.txt --phases out/phases2.log --cpu-log out/cpu2_samples.log \
  --meta out/pm_meta2.log --bench-glob 'out/bench_P4s_*.json' \
  --report out/p4_ane_report.md --title 'P4 ANE 复验（静态 N 模型）'
```

## 已知坑

- osascript root 进程写出的文件属主是 root；admin 脚本末尾已 chmod 644。
- powermetrics 必须显式 `-s cpu_power,gpu_power,ane_power`；M5 输出只有聚合 CPU/GPU/ANE Power，无分簇瓦数、无 DRAM 行。
- `--show-process-gpu` 在显式 `-s` 下不产出进程表（本次实测），GPU 归因改用 ps 采样 qemu CPU%。
- **AzurLaneConfig 每次 setattr 都会写盘**（"memory only" 注释是错的：曾把部署 config 落成
  `ScreenshotMethod='scrcpy'`，部署版 scrcpy 在 Android 16 会崩）。凡动配置，快照 + finally 恢复。
- scrcpy 1.20 握手：`DesktopConnection.open` 无论 control 与否都 accept 两条连接（video→control），
  dummy byte 在第一条后发，设备 meta 与视频流在两条都建立后才开始。只建 video 会永久卡在第二个 accept。
- Android 16（API 36）移除了 `SurfaceControl.createDisplay(String,boolean)`：scrcpy 1.20 server
  反射调用即 `AssertionError` 死亡（发生在编码器分配成功之后）——scrcpy 1.20 在 Android 16 无解，
  唯一出路是升级 server（payload 仅捆 v1.20）。
- OCR 模型 ONNX 签名 N、W 均动态；只桶 W（N 仍动态）或静态 N+动态 W 都不落 ANE（ane_power 恒 0）。
  全静态重导出（out/azur_lane_staticn_w640.onnx，W 定宽 640）+ fixed640 模式实测落 ANE ~311mW，
  复验走 run_p4_admin.sh。
- 相位功耗对比存在"位移效应"：探针负载抢 qemu 周期，负载相位的总 CPU 功耗可能反而低于空闲相位，解读时必须对照 qemu CPU% 列。
