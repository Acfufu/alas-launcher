# Draft 2 — Google Issue Tracker（八号：宿主侧 vid readback SEGV）

**目标**: Google Issue Tracker（Android Studio & Tools → Emulator 组件；用户浏览器提交，需 Google 账号）
**状态**: 草稿，未发送。提交前需用户过目。

---

## Title

Emulator 37.2.12 macOS arm64: any host-side video/capture (Running Devices mirror, screen record, screenshot button, ADBExtras vid probe) SEGVs in `FrameBuffer::Impl::sendReadbackWorkerCmd` when the AVD uses GuestAngle (no host GL)

## Body

### Environment

- Android emulator **37.2.12.0 (build_id 16428233)**, macOS Apple Silicon (arm64), darwin 27.2
- AVD launched with `-feature GuestAngle` (guest-side ANGLE → Metal; no host GL context for the guest surface)
- Graphics backend: gfxstream

### What happens

Any host-side capture path that opens a **vid handle** against the emulator while GuestAngle is active crashes the whole emulator with `EXC_BAD_ACCESS (SIGSEGV)` inside the readback worker:

```
Thread Crashed:
0   libgfxstream_backend.dylib  0x…  gfxstream::host::FrameBuffer::Impl::sendReadbackWorkerCmd(gfxstream::host::FrameBuffer::Impl::Readback const&)
1   libgfxstream_backend.dylib  0x…  gfxstream::base::WorkerThread<gfxstream::host::FrameBuffer::Impl::Readback>::ThreadLoop()
2   ...                          _pthread_start
```

(From `qemu-system-aarch64-2026-10-27-054040.ips` and `…-132335.ips`.)

Observed triggers, all on 2026-10-27:

1. Android Studio **Running Devices** mirror / the emulator window's record & screenshot buttons (host-side `aemu-recording` vid handle).
2. A third-party controller (MaaFramework 5.15.0-beta.1 "AVDExtras") merely **enumerating devices** — it opens a vid probe against an AVD it never connects to; both crashes above (05:40, 13:23) landed on an AVD that was otherwise idle and healthy.

On AVDs running **native guest GL** (no GuestAngle), the same vid probe crashes differently — in compose teardown instead of readback:

```
0   AppleMetalOpenGLRenderer    GLRResourceList::addFence(GLRResource*)
1   AppleMetalOpenGLRenderer    GLDContextRec::flushContext(bool)
2   libgfxstream_backend.dylib  translator::egl::eglMakeCurrent(...)
3   … DisplaySurfaceGlContextHelper::teardownContext()
4   … FrameBuffer::Impl::postImpl(...) ← postImplSync ← compose ← rcCompose
```

(`…-134601.ips`, 15 s after the device-enumeration vid probe.)

Notably, one AVD on this machine (an Endfield instance, same emulator build, same day) survived the identical vid probe twice — so the crash depends on the guest rendering configuration, not purely on the probe.

### What I'd expect

Host-side capture should check whether the guest surface can actually be read back (GuestAngle ⇒ no host GL to read from) and degrade gracefully — return an error to the caller / disable the video stream — instead of dereferencing a null/absent context in the readback worker and taking the whole VM down. That a mere device **scan** from a companion tool can kill a running, otherwise-healthy emulator makes this more than a cosmetic issue.

### Workaround we currently use

- Never host-capture GuestAngle AVDs (screenshots go through `adb shell screencap` instead)
- Keep the third-party scanner away from AVDs it won't drive (mutex on our side)

### Repro sketch

1. Create an arm64 AVD, launch with `-feature GuestAngle`, let it finish booting.
2. Start a host-side capture (e.g. the emulator window's screenshot/record button, or Running Devices mirroring from Studio).
3. Emulator process dies with SIGSEGV in `sendReadbackWorkerCmd` (GuestAngle) or in compose teardown (native GL).

Full `.ips` files available on request.

---

## 附：提交前用户过目点

- Issue Tracker 组件建议选 **Developer Tools → Android Studio → Emulator**（提交时按实际页面组件树选）。
- 第三方工具点名 MaaFramework 5.15.0-beta.1 AVDExtras——事实陈述，无责备措辞。如不想点名可改为 "a companion automation tool"。
- .ips 时间戳与栈已核对（05:40:40 / 13:23:35 / 13:46:01）。
