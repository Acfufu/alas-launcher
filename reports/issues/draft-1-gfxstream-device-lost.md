# Draft 1 — gfxstream（四号：AGX GPU timeout → device lost → teardown SEGV）

**目标**: https://github.com/google/gfxstream （issue）
**状态**: 草稿，未发送。发送前需用户过目。

---

## Title

macOS Apple Silicon: AGX GPU timeout (`kIOGPUCommandBufferCallbackErrorTimeout`) marks the VkDevice lost; gfxstream RenderThread then SEGVs in `eglMakeCurrent`/`teardownContext` instead of surfacing a recoverable error

## Body

### Environment

- Android emulator **37.2.12.0 (build_id 16428233)** on macOS (Apple Silicon, M-series; GPU family AGX G17G), arm64
- Graphics backend: **gfxstream** (`libgfxstream_backend.dylib` shipped with the emulator), MoltenVK underneath
- Guest renders via the GuestAngle path (host-side GL absent for the guest surface)
- Host macOS 27.2 (darwin 27.2.0, arm64)

### What happens

Under sustained rendering (an idle game farm running for hours), the Apple GPU driver kills a `MTLCommandBuffer` with a GPU timeout. MoltenVK then reports the VkDevice lost:

```
[mvk-error] VK_TIMEOUT: Lost VkDevice after MTLCommandBuffer "vkQueueSubmit MTLCommandBuffer on Queue 0-0" execution failed (code 2): Caused GPU Timeout Error (00000002:kIOGPUCommandBufferCallbackErrorTimeout)
Encountered device lost.
```

Immediately after, the gfxstream RenderThread dereferences freed/invalid state during context teardown and the whole emulator process dies with `EXC_BAD_ACCESS (SIGSEGV)`:

```
Thread X Crashed (RenderThread):
0   AppleMetalOpenGLRenderer        0x…  GLRResourceList::addFence(GLRResource*)
1   AppleMetalOpenGLRenderer        0x…  GLDContextRec::flushContext(bool)
2   libgfxstream_backend.dylib      0x…  translator::egl::eglMakeCurrent(void*, void*, void*, void*)
3   libgfxstream_backend.dylib      0x…  gfxstream::host::gl::(anonymous namespace)::DisplaySurfaceGlContextHelper::teardownContext()
4   libgfxstream_backend.dylib      0x…  gfxstream::host::gl::EmulationGl::drainRenderThreadResources()
5   libgfxstream_backend.dylib      0x…  gfxstream::host::RenderThread::main()
6   libgfxstream_backend.dylib      0x…  gfxstream::base::Thread::thread_main(void*)
7   libsystem_pthread.dylib         0x…  _pthread_start
```

(From `qemu-system-aarch64-…-2026-10-24-044957.ips`; identical signature across at least 7 crashes on 2026-10-24/25/26.)

### What I'd expect

The root trigger is host-side (Apple AGX timing out a command buffer — likely an Apple driver issue we cannot fix here). But from gfxstream's side, a `VK_ERROR_DEVICE_LOST` / lost-device condition currently flows into `eglMakeCurrent` → `teardownContext()` → `drainRenderThreadResources()` and crashes the process on teardown with what looks like use-after-free of GL resources (`GLRResourceList::addFence`).

Expected: after a lost device, teardown/drain paths should tolerate the invalid context and let the emulator surface a recoverable error (or at least shut down cleanly), instead of a hard SEGV that takes the whole VM with it.

### Repro

Not deterministic on demand; it recurs every few hours of continuous guest rendering on this machine (7 crashes over 3 days). Happy to attach full `.ips` files or emulator verbose logs if useful.

### Questions

- Is there a known recommended handling when the host reports device-lost mid-session (e.g. a flag to abort the render thread without draining)?
- Any diagnostics you'd want captured on the next occurrence?

---

## 附：提交前用户过目点

- 机器描述只写 M-series/AGX G17G，无序列号/用户名。✔
- 崩溃栈为 2026-10-24-044957.ips faulting thread 原帧。完整 .ips 可后续 attach。
- `Encountered device lost.` 与 mvk-error 行取自 AndroidStudio idea.log 2026-10-26 原话。
