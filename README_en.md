**| English | [简体中文](README.md) |**

# ALAS Launcher — a native cross-platform launcher for AzurLaneAutoScript

A Tauri 2 (Rust) desktop launcher that starts [AzurLaneAutoScript](https://github.com/LmeSzinc/AzurLaneAutoScript) (ALAS) natively on Windows, macOS, and Linux — no translation layer, no Docker. It prepares the runtime environment, updates the ALAS repository, and opens the local Web UI for you.

![ALAS Launcher hybrid hero: on the left, the launcher title, tech badges, platform cards, and the "no Docker, unzip and play" description; on the right, a card embedding a real screenshot of the ALAS Web UI on macOS, labeled 127.0.0.1:22267](assets/readme/hero-en.webp)

### Menu-bar overview (macOS)

A macOS-native adaptation: the menu bar hosts the ALAS ship icon, folding scheduler status, the live task list, and the start/stop toggle into one native menu — **inspect and control the current state without opening a window**.

<p align="center"><img src="assets/readme/tray-menu.png" width="56%" alt="Real macOS menu-bar overview: ALAS scheduler status, task list, and start/stop toggle"></p>
<p align="center"><sub>Menu-bar task list and scheduler toggle · <a href="screenshots/mac-en.webp">Full Web UI screenshot</a></sub></p>

- **Live task list**: consistent with the Web UI scheduler — real ALAS tasks read from `config/alas.json`, grouped by `Running / Queued / Waiting`, with the next scheduled time; up to 3 per group to keep the menu compact, auto-refreshed every 3 seconds (manual refresh too).
- **One-click scheduler toggle**: the menu toggle controls the ALAS scheduler, not the backend process — stopping the scheduler keeps the Web UI alive; if the backend is not running, the toggle starts it first.
- **macOS only**: the menu-bar icon is currently enabled only on macOS; Windows / Linux builds are unaffected.

## How it works

The launcher automates the whole chain — prepare the environment, update the repository, launch the Web UI:

![ALAS Launcher launch-flow animation: initialize the environment (inject PATH and LD_LIBRARY_PATH, read WebuiPort default 22267), update the repository (clean config, pull latest ALAS), start the Web service (gui.py --host 127.0.0.1 --port 22267), then ready (WebView opens the local ALAS Web UI)](assets/readme/workflow-animated-en.svg)

1. **Initialize**: prepares the runtime environment (PATH, LD_LIBRARY_PATH, etc.) and reads the Web UI port from `config/deploy.yaml` (default `22267`).
2. **Update**: cleans the ALAS config directory and pulls the latest ALAS repository.
3. **Launch**: starts `gui.py` on the local port (`gui.py --host 127.0.0.1 --port <configured port>`).
4. **Ready**: the Tauri WebView opens the local Web UI, showing `Ready · 127.0.0.1:<port>`.

If the repository update fails, the splash screen keeps showing the error; quitting the launcher terminates the backend process. adb replacement, pip auto-update, and remote access are not part of the launcher's responsibility.

## Differences from the official launcher

![Startup behavior comparison: the official launcher kills existing processes, updates pip, updates Electron resources, and restarts adb; ALAS Launcher only updates the repository with no other side effects](assets/readme/compare-en.svg)

| Capability | Official launcher | This launcher |
| --- | --- | --- |
| Platforms | Single platform | The same codebase natively supports Windows, macOS, and Linux |
| macOS task overview | None | Menu-bar task list + native notifications (scheduler abnormal-death alert on by default, task-complete optional) |
| Scheduler control channel | Web page button | Control API patch (HTTP, same ProcessManager as the web page; degrades to process-level control with password/SSL) |
| Startup behavior | Kills existing processes, updates pip, updates Electron resources, restarts adb | Only updates the repository + cleans up stale ALAS processes from the previous run of this install — no other side effects |
| Repeated launch | Opens a new window | Single instance: refocuses the existing window |
| pip auto-update | Yes | Disabled (Python package versions differ slightly from the official version; does not affect usage) |
| adb restart / replacement | Yes | Not implemented |

Cleanup behavior is described in the table above; the comparison diagram still shows the previous wording.

The directory layout is also adjusted — see [Directory structure and environment variables](#directory-structure-and-environment-variables).

## Quick start

Download the archive for your OS and CPU architecture from the [Releases page](https://github.com/Acfufu/alas-launcher/releases), unpack it, and start it on your platform.

> [!IMPORTANT]
> The current remote Release (`v0.1.0`) is a manually built draft, not yet declared a stable public install package — verify it yourself before use.

| Platform | Start | Notes |
| --- | --- | --- |
| Windows | Run `alas-launcher.exe` | Windows 7 / 8 / 10 need [WebView2](https://developer.microsoft.com/en-us/microsoft-edge/webview2) installed first |
| macOS | Open `AzurLaneAutoScript.app` | Unsigned; if Gatekeeper blocks it, run the command below |
| Linux | Run `alas-launcher` | Requires `libwebkit2gtk-4.1` and a recent `glibc` (CI uses Ubuntu 22.04); missing dependencies may prevent the launcher from starting while ALAS itself usually still runs |

If macOS Gatekeeper blocks the first launch:

```bash
xattr -dr com.apple.quarantine AzurLaneAutoScript.app   # remove the quarantine attribute to bypass Gatekeeper
```

**Success condition**: once ready, the launcher opens the local Web UI showing `Ready · 127.0.0.1:22267`.

## Configuration

The Web UI listens on port `22267` by default; the launcher reads `config/deploy.yaml` at startup, and the port is set in `Deploy.Webui.WebuiPort`.

- **Language**: the language switch only affects launcher UI (menu bar, tray, stop page); the ALAS web page language always follows `Gui.Language` in `config/deploy.yaml`, and the two may be out of sync (by design).
- **Password / SSL**: after configuring `Deploy.Webui.Password` / `WebuiSSLKey` / `WebuiSSLCert`, the menu-bar scheduler toggle degrades to process-level control (the control API can no longer drive the scheduler), and the tray status line appends a "password/SSL configured, process-level control only" notice.

## Build and release

> For maintainers. End users do not need to build — download from [Releases](https://github.com/Acfufu/alas-launcher/releases).

<details>
<summary>Build the macOS launcher shell</summary>

Dependencies: [Rust](https://rustup.rs) (stable), Node.js (provides npx), Xcode Command Line Tools.

```bash
npx --yes @tauri-apps/cli@2 build --bundles app         # build the macOS launcher shell
```

The artifact lands in `target/release/bundle/macos/AzurLaneAutoScript.app`.

**Assemble the full payload.** The artifact above is only the launcher shell — it does not contain ALAS itself. A fully usable version needs the payload (Python toolkit / git / adb / ALAS repository) inside `Contents/AzurLaneAutoScript`. You can copy it from an existing installation:

```bash
APP=target/release/bundle/macos/AzurLaneAutoScript.app
cp -R /Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript "$APP/Contents/"   # copy ALAS from an existing install
```

Full artifacts go to the `release/` directory (gitignored, to avoid accidental commits):

```bash
cp -R target/release/bundle/macos/AzurLaneAutoScript.app release/   # full artifacts go here (gitignored)
```

![ALAS payload cross-platform directory layout: Windows is AzurLaneAutoScript/alas-launcher.exe with toolkit (git and adb.exe inside); macOS is AzurLaneAutoScript.app/Contents/AzurLaneAutoScript with Contents/MacOS/alas-launcher; Linux is AzurLaneAutoScript/alas-launcher with toolkit; the bottom compares Unix and Windows PATH/LD_LIBRARY_PATH differences](assets/readme/payload.svg)

**Release.** Releasing is a manual flow: build the full `.app` (with payload) locally, place it in `release/`, then publish. The version must match `version` in `tauri.conf.json`:

```bash
git tag v0.1.0            # version must match tauri.conf.json version
git push origin v0.1.0
gh release create v0.1.0 release/AzurLaneAutoScript.app --title "v0.1.0" --notes "..."
```

<!-- end of build and release -->
</details>

## Directory structure and environment variables

Paths below are relative to the ALAS root; `toolkit` is the Python environment directory (venv-like):

| Component | Windows | macOS | Linux |
| --- | --- | --- | --- |
| ALAS root | `AzurLaneAutoScript` | `AzurLaneAutoScript.app/Contents/AzurLaneAutoScript` | `AzurLaneAutoScript` |
| Launcher | `AzurLaneAutoScript/alas-launcher.exe` | `AzurLaneAutoScript.app/Contents/MacOS/alas-launcher` | `AzurLaneAutoScript/alas-launcher` |
| Python | `toolkit` | `toolkit` | `toolkit` |
| Git | MinGit unpacked into `toolkit/git` | Unix layout inside `toolkit` | Unix layout inside `toolkit` |
| Adb | `toolkit/adb.exe` | `toolkit/bin/adb` | `toolkit/bin/adb` |

The launcher adds the following environment variables:

- **Unix**: `toolkit/bin`, `toolkit/libexec/git-core`, and `toolkit/lib` (`LD_LIBRARY_PATH`).
- **Windows**: `toolkit`, `toolkit/Scripts`, `toolkit/git/cmd`.

## Limitations, troubleshooting, and license

### Limitations

- The launcher shell does not contain the ALAS payload; it must be assembled manually (see [Build and release](#build-and-release)).
- The macOS app is unsigned; the first launch requires manually clearing quarantine.
- The menu-bar overview (task list / scheduler toggle) is enabled only on macOS.
- The scheduler toggle depends on the ALAS control API patch (anchor: `module/webui/fastapi.py`, unchanged since 2022-04-14); if the anchor breaks, the tray scheduler toggle degrades to process-level control (not a silent no-op).
- Linux requires `libwebkit2gtk-4.1` and a recent `glibc`.
- adb restart/replacement is not implemented; pip auto-update is disabled.
- The remote Release is a draft, not yet declared a stable public install package.

### Troubleshooting

| Symptom | Fix |
| --- | --- |
| macOS says the app is damaged or cannot be opened | Run `xattr -dr com.apple.quarantine AzurLaneAutoScript.app` |
| Linux launcher fails to start, but ALAS itself runs | Install `libwebkit2gtk-4.1`, or upgrade system `glibc` |
| Port in use or you want a different port | Edit `Deploy.Webui.WebuiPort` in `config/deploy.yaml` (default `22267`) |
| Menu-bar task list empty or shows "Tasks: unavailable" | Make sure the ALAS backend is running (the menu can start it); task data comes directly from `config/alas.json` |
| Stale cleanup failed / splash shows stale process error | Kill the listed pids per the error page, or change Deploy.Webui.WebuiPort in config/deploy.yaml and relaunch |

### License

Because ALAS uses GPLv3, this launcher is also GPLv3. Most dependencies are permissively licensed (Apache-2.0, BSD-3-Clause, etc.); see each upstream repository for details.
