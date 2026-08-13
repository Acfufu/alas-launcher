**| [English](README_en.md) | 简体中文 |**

# ALAS Launcher — AzurLaneAutoScript 的跨平台原生启动器

基于 Tauri 2（Rust）的桌面启动器，在 Windows、macOS、Linux 上原生启动 [AzurLaneAutoScript](https://github.com/LmeSzinc/AzurLaneAutoScript)（ALAS）：不经过转译层、不依赖 Docker，启动后自动准备环境、更新仓库并打开本地 Web UI。

## 界面一览

| macOS | Windows |
| --- | --- |
| ![macOS 上 ALAS Launcher 的桌面 Web UI 截图，显示 ALAS 首页任务面板](screenshots/mac-cn.webp) | ![Windows 上 ALAS Launcher 的桌面 Web UI 截图，显示 ALAS 首页任务面板](screenshots/win-cn.webp) |

### macOS 菜单栏速览

macOS 上带菜单栏图标：不用打开窗口就能查看 ALAS 任务列表、启停调度器。

![ALAS 菜单栏动图：点击菜单栏舰船图标弹出原生菜单，显示调度器运行状态与启动/停止开关（Web UI 保持存活）；随后展示 Web UI 调度器按「运行中 / 队列中 / 等待中」分组的真实 ALAS 任务列表（含下次调度时间）与主窗口](assets/readme/tray-zh.gif)

- **任务列表实时可见**：与 Web UI 调度器一致，按「运行中 / 队列中 / 等待中」分组展示真实 ALAS 任务（数据来自 `config/alas.json`）与下次调度时间；每组最多 3 条，3 秒自动刷新，也可手动刷新。
- **调度器一键启停**：菜单开关控制的是 ALAS 调度器而非后端进程——停止调度器后 Web UI 保持存活；后端未运行时点击开关会先启动后端。
- **仅 macOS**：菜单栏图标目前只在 macOS 上启用，Windows / Linux 构建不受影响。

动图无法播放时，可查看[静态截图版](screenshots/mac-cn.webp)。

## 它能做什么

启动器把「准备环境 → 更新仓库 → 启动 Web UI」整条链路自动化：

![ALAS 启动器工作流：启动器 → 环境配置（PATH/LD_LIBRARY_PATH）→ 读取 config/deploy.yaml 的 WebuiPort（默认 22267）→ 清理配置并更新 ALAS Git 仓库 → 启动 gui.py → Tauri WebView 导航到本地 Web UI；更新失败时启动画面显示错误，窗口关闭时后端被终止](assets/readme/workflow-zh.svg)

1. **初始化**：准备运行环境（PATH、LD_LIBRARY_PATH 等），从 `config/deploy.yaml` 读取 Web UI 端口。
2. **更新**：清理 ALAS 配置目录，拉取 ALAS 仓库更新。
3. **启动**：在本机端口启动 `gui.py`（`gui.py --host 127.0.0.1 --port <配置端口>`）。
4. **就绪**：Tauri WebView 打开本地 Web UI，显示 `Ready · 127.0.0.1:<端口>`。

仓库更新失败时，启动画面会继续显示错误信息；退出启动器时，后端进程会被终止。adb 替换、pip 自动更新和远程访问不属于启动器职责。

## 与官方版的差异

| 能力 | 官方版 | 本版本 |
| --- | --- | --- |
| 平台 | 单一平台 | 同一套代码原生支持 Windows、macOS、Linux |
| macOS 任务速览 | 无 | 菜单栏任务列表 + 原生通知（调度器异常死亡默认提醒、任务完成可选） |
| 调度器控制通道 | 网页按钮 | 控制 API 补丁（HTTP，与网页同一 ProcessManager；密码/SSL 时降级进程级控制） |
| 启动时行为 | 杀已有进程、更新 pip、更新 Electron 资源、重启 adb | 只更新仓库，无其他副作用操作 |
| 重复启动 | 打开新窗口 | 单实例：重新聚焦已有窗口 |
| pip 自动更新 | 有 | 已禁用（Python 包版本与官方版略有差异，但不影响使用） |
| adb 重启 / 替换 | 有 | 未实现 |

目录结构也有调整，见[目录结构与环境变量](#目录结构与环境变量)。

## 快速开始

到 [Releases 页面](https://github.com/Acfufu/alas-launcher/releases) 下载对应系统和 CPU 架构的压缩包，解压后按平台启动。

> [!IMPORTANT]
> 当前远程 Release（`v0.1.0`）是手动构建的草稿版本，尚未声明为稳定公开安装包，使用前请自行验证。

| 平台 | 启动方式 | 注意事项 |
| --- | --- | --- |
| Windows | 运行 `alas-launcher.exe` | Windows 7 / 8 / 10 需先安装 [WebView2](https://developer.microsoft.com/zh-cn/microsoft-edge/webview2) |
| macOS | 打开 `AzurLaneAutoScript.app` | 未签名；若被 Gatekeeper 拦截，执行下方命令 |
| Linux | 运行 `alas-launcher` | 依赖 `libwebkit2gtk-4.1` 与较新的 `glibc`（CI 使用 Ubuntu 22.04）；缺少依赖时启动器可能无法运行，ALAS 本体通常不受影响 |

macOS 首次打开如被 Gatekeeper 拦截：

```bash
xattr -dr com.apple.quarantine AzurLaneAutoScript.app   # 移除隔离属性，绕过 Gatekeeper
```

**成功标志**：启动器就绪后自动打开本地 Web UI，页面显示 `Ready · 127.0.0.1:22267`。

## 配置

Web UI 默认监听端口为 `22267`，启动器启动时读取 `config/deploy.yaml`，端口在其中的 `Deploy.Webui.WebuiPort` 修改。

- **语言**：语言切换只影响启动器 UI（菜单栏、托盘、停止页）；ALAS 网页语言始终跟随 `config/deploy.yaml` 的 `Gui.Language`，二者可能不同步（设计如此）。
- **密码 / SSL**：为 WebUI 配置 `Deploy.Webui.Password` / `WebuiSSLKey` / `WebuiSSLCert` 后，菜单栏调度器开关退化为进程级控制（无法通过控制 API 驱动调度器），托盘状态行会追加「密码/SSL 已配置，仅进程级控制」降级提示。

## 构建与发布

> 面向维护者。终端用户不需要构建——直接从 [Releases](https://github.com/Acfufu/alas-launcher/releases) 下载即可。

<details>
<summary>构建 macOS 启动器外壳</summary>

依赖：[Rust](https://rustup.rs)（stable）、Node.js（提供 npx）、Xcode Command Line Tools。

```bash
npx --yes @tauri-apps/cli@2 build --bundles app         # 构建 macOS 启动器外壳
```

产物在 `target/release/bundle/macos/AzurLaneAutoScript.app`。

**拼装完整 payload。** 上面的产物只是启动器外壳，不含 ALAS 本体。完整可用的版本需要把 payload（Python toolkit / git / adb / ALAS 仓库）放进 `Contents/AzurLaneAutoScript`。可以从已有安装拷贝：

```bash
APP=target/release/bundle/macos/AzurLaneAutoScript.app
cp -R /Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript "$APP/Contents/"   # 从已有安装拷贝 ALAS
```

完整产物统一放到 `release/` 目录（已 gitignore，避免误提交）：

```bash
cp -R target/release/bundle/macos/AzurLaneAutoScript.app release/   # 完整产物统一放这里（已 gitignore）
```

![ALAS payload 跨平台目录结构：Windows 为 AzurLaneAutoScript/alas-launcher.exe 与 toolkit（内含 git 和 adb.exe）；macOS 为 AzurLaneAutoScript.app/Contents/AzurLaneAutoScript 与 Contents/MacOS/alas-launcher；Linux 为 AzurLaneAutoScript/alas-launcher 与 toolkit；底部对比 Unix 与 Windows 的 PATH/LD_LIBRARY_PATH 差异](assets/readme/payload-zh.svg)

**发布。** 发布是手动流程：本地构建完整 `.app`（含 payload），放进 `release/`，然后发版。版本号必须与 `tauri.conf.json` 的 `version` 一致：

```bash
git tag v0.1.0            # 版本号与 tauri.conf.json 的 version 一致
git push origin v0.1.0
gh release create v0.1.0 release/AzurLaneAutoScript.app --title "v0.1.0" --notes "..."
```

<!-- 构建与发布结束 -->
</details>

## 目录结构与环境变量

以下路径以 ALAS 根目录为基准，`toolkit` 为 Python 环境目录（类似 venv 结构）：

| 组件 | Windows | macOS | Linux |
| --- | --- | --- | --- |
| ALAS 根目录 | `AzurLaneAutoScript` | `AzurLaneAutoScript.app/Contents/AzurLaneAutoScript` | `AzurLaneAutoScript` |
| 启动器 | `AzurLaneAutoScript/alas-launcher.exe` | `AzurLaneAutoScript.app/Contents/MacOS/alas-launcher` | `AzurLaneAutoScript/alas-launcher` |
| Python | `toolkit` | `toolkit` | `toolkit` |
| Git | MinGit 解压到 `toolkit/git` | Unix 目录结构装入 `toolkit` | Unix 目录结构装入 `toolkit` |
| Adb | `toolkit/adb.exe` | `toolkit/bin/adb` | `toolkit/bin/adb` |

启动器会加入以下环境变量：

- **Unix**：`toolkit/bin`、`toolkit/libexec/git-core`，以及 `toolkit/lib`（`LD_LIBRARY_PATH`）。
- **Windows**：`toolkit`、`toolkit/Scripts`、`toolkit/git/cmd`。

## 限制、故障排查与许可

### 限制

- 启动器外壳不含 ALAS payload，需要手动拼装（见[构建与发布](#构建与发布)）。
- macOS 应用未签名，首次打开需要手动解除 quarantine。
- 菜单栏速览（任务列表/调度器启停）仅在 macOS 上启用。
- 调度器启停依赖对 ALAS 的 control API 补丁（锚点为 `module/webui/fastapi.py`，自 2022-04-14 起未变）；若锚点失效，托盘调度器开关降级为进程级控制（非静默 no-op）。
- Linux 依赖 `libwebkit2gtk-4.1` 与较新的 `glibc`。
- adb 重启/替换未实现；pip 自动更新已禁用。
- 远程 Release 为草稿，尚未声明为稳定公开安装包。

### 故障排查

| 现象 | 处理 |
| --- | --- |
| macOS 提示应用已损坏或无法打开 | 执行 `xattr -dr com.apple.quarantine AzurLaneAutoScript.app` |
| Linux 启动器无法启动，但 ALAS 本体可运行 | 安装 `libwebkit2gtk-4.1`，或升级系统 `glibc` |
| 端口被占用或想更换端口 | 修改 `config/deploy.yaml` 的 `Deploy.Webui.WebuiPort`（默认 `22267`） |
| 菜单栏任务列表为空或显示「Tasks: unavailable」 | 确认 ALAS 后端在运行（菜单可一键启动）；任务数据直接来自 `config/alas.json` |

### 许可

因为 ALAS 使用 GPLv3，本启动器也使用 GPLv3。多数依赖为 Apache-2.0、BSD-3-Clause 等宽松许可，具体请查阅各上游仓库。
