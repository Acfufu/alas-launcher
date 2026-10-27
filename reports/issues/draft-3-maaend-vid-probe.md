# Draft 3 — MaaEnd（九/十号：设备扫描开 vid 探测杀 AVD）

**目标**: https://github.com/MaaEnd/MaaEnd （已定位；建议抄送 https://github.com/MaaXYZ/MaaFramework —— vid probe 行为在 MaaFramework 5.15.0-beta.1 的 AVDExtras 控制器里）
**状态**: 草稿，未发送。提交前需用户过目。

---

## 标题

设备扫描/刷新列表会对未连接的模拟器开启 AVDExtras vid 探测，触发 Android emulator 37.2.12 (macOS) 宿主侧崩溃，杀死正在运行的 AVD

## 正文

### 环境

- macOS（Apple Silicon, arm64），Android emulator **37.2.12.0 (build_id 16428233)**，gfxstream 后端
- MaaEnd 内置 **MaaFramework 5.15.0-beta.1** 的 AVDExtras 控制器
- 三台 AVD：Azurelane（`-feature GuestAngle`，guest ANGLE→Metal）、Arknight（原生 guest GL）、Endfield

### 现象

MaaEnd 的设备列表刷新（扫描）会对**它并未连接**的 AVD 开启 vid 探测（AVDExtras 的视频流句柄）。在 macOS 模拟器上，这条宿主侧捕获路径本身有崩溃缺陷（已另行报给 Google issuetracker），结果是"扫一眼设备列表，别的 AVD 死了"：

- 2026-10-27 **05:40:40** 与 **13:23:35**：扫描对 Azurelane（GuestAngle）开 vid → qemu `EXC_BAD_ACCESS` 于 `FrameBuffer::Impl::sendReadbackWorkerCmd`（readback worker 线程），模拟器整进程死亡。该 AVD 此前运行正常、与 MaaEnd 无连接。
- 2026-10-27 **13:45:43** 扫描对仅剩的 Arknight（原生 GL）开 vid → 15 秒后（**13:46:01**）qemu 崩于 compose teardown（`rcCompose → postImpl → teardownContext → eglMakeCurrent → GLRResourceList::addFence` SEGV）。原生 GL 路径同样不免疫。
- 同日 Endfield 吃过同款 vid 探测两次而幸存（与其余两台同版本模拟器）——说明崩溃依赖 guest 渲染配置，但"扫描即开 vid"是把所有 AVD 暴露在这个风险下的行为。

崩溃栈（.ips faulting thread 原帧）：

```
# GuestAngle AVD（readback 路径）
0  gfxstream::host::FrameBuffer::Impl::sendReadbackWorkerCmd(...)
1  gfxstream::base::WorkerThread<FrameBuffer::Impl::Readback>::ThreadLoop()

# 原生 GL AVD（compose teardown 路径）
0  AppleMetalOpenGLRenderer  GLRResourceList::addFence(GLRResource*)
1  AppleMetalOpenGLRenderer  GLDContextRec::flushContext(bool)
2  libgfxstream_backend.dylib  translator::egl::eglMakeCurrent(...)
3  … DisplaySurfaceGlContextHelper::teardownContext()
4  … FrameBuffer::Impl::postImpl(...) ← compose ← rcCompose
```

### 请求

设备**扫描/枚举**阶段不要对 AVD 开启 vid 探测——枚举只需要 adb 设备列表；vid 视频流应在**真正连接并开始控制**某台设备时才建立（并在断开后关闭）。如果 MaaEnd 侧短期改不了，希望提供一个"扫描不探测视频流"的开关。

我们侧目前的规避：MaaEnd 运行期间手动禁止其他 AVD 启动（互斥规则），Endfield 之外不同场。这不是理想解——两套工具本可以同机共存。

### 时间线（取证）

- 13:45:43 MaaEnd 刷新设备列表（进程活动与 adb 调用序列）
- 13:45:58 前后 对 5556（Arknight）开 vid 探测
- 13:46:01 qemu-system-aarch64 SIGSEGV（.ips 134601）

（另有 05:40:40 / 13:23:35 两次同签名崩溃，均落在被扫描但未连接的 Azurelane 上。）

---

## 附：提交前用户过目点

- MaaEnd 版本号：草稿只写了内置 MaaFramework 5.15.0-beta.1；若想精确，可在 MaaEnd「关于」里查应用版本后补一行。
- 措辞已对齐"报事实、不指摘"（崩溃根因在模拟器宿主侧捕获路径，MaaEnd 的行为是触发器）。
- 是否同时抄送 MaaXYZ/MaaFramework（AVDExtras 实现在 maafw 侧）——建议：先发 MaaEnd，看维护者引导。
