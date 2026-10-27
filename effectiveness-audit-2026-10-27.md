# ALAS webapp 生效性审查报告（Effectiveness Audit）

> **修复状态（2026-10-28 全量收尾完成）**：30 条发现中 **28 条已修毕**，2 条按裁决记录在案收窄/接受。
> | 批次 | payload 提交 | 内容 |
> |---|---|---|
> | 上一批 | `a5af1c05f` | H1/H2/H3/H4 + M1/M2/M7 |
> | 批次1 | `fd9660486` | M3/M4/M10/L6/L7(被H1覆盖)/L5 + **L3 提前修**（它吞掉 M10 的诚实 404）|
> | 批次2 | `38fd31857` | M5/M11/M12/L9/L12；L11 经核对已被 H2 的 running_func die-flip 覆盖，残余（双开客户端）接受 |
> | 批次3 | `dffcd0074` | M6/M8/M9（import 放宽为可创建实例=备份恢复正道 + 清 Serial；schema 按实例 PackageName 解析 server；mod 实例只读化 + /configs 与 /status 口径统一）|
> | 批次4 | `1d8f5ae80` | L1/L2/L4/L8/L10/L13/L14(lang 动态化；版本戳已由 Update 页 /update/status 覆盖) |
>
> 收窄/接受项：**L15** phantom 'alas' = 全新安装引导路径，按设计保留（M10 校验后拼错名已无法借道）；**L11** 残余仅在双开客户端时可见。每批均经 pytest 全绿 + svelte-check/vitest + L2 隔离副本实测（探针脚本 /tmp/l2_probe_b1..b4.py）。
> **部署注记**：`webapp/dist` 被 gitignore，git push 不装机——前端 dist 需 rsync 进 app bundle（已同步至 1d8f5ae80 对应构建）；bundle 后端文件由 launcher 下次启动 fetch+reset 装机。

- **审查日期**：2026-10-27 设计，2026-10-28 执行
- **对象**：`~/Codehub/alas-payload` @ HEAD `2f63e4af1`——webapp/（Svelte：Home/Manage/Settings/Develop + 组件）↔ module/webui/api/ 全部路由 + SSE 事件流
- **背景**：2f63e4af1 修复了工具页 `toggleTool` 漏传 `func`、`ok:false` 被吞导致"点启动没生效"；本审查把同类问题在全表面系统性清点
- **方法**：机械枚举对账 → 逐条双向追踪（五类打标：A 参数契约断裂 / B 吞错伪成功 / C 语义偏差 / D 产物漂移 / E 状态不持久或残留）→ 前端锚定 + 后端锚定双审交叉 → ≥20% 抽查验证 → 三级实弹验证
- **完成度**：五段全部完成。L1 实机 HTTP 与 L3 真实例探针因红线冲突改道（见 §c.1），同等验证已在文件级比对与 L2 隔离副本完成。

**双审分歧裁决**（方法论价值，详见附录）：两审查人共报 39 条原始发现，合并去重后 30 条；其中 2 条经原文抽查 + 副本实测被**否决/收窄**——交叉双审 + 实测裁决机制被证明必要。

---

## a) 对账总表（动作 × 路由 × 失败表现 × 分类 × 实测等级）

实测等级：`静态`=代码追踪；`L2`=隔离副本实测；`文件`=只读文件级比对。

| # | 动作（UI 侧） | 路由 | 失败时用户看到什么 | 类 | 等级 |
|---|---|---|---|---|---|
| 1 | 启动页 bootstrap / 各视图 refreshStatus | GET /status | main.ts 有 catch；视图内无 catch → 侧栏空白 | B | 静态 |
| 2 | SSE /sse（status/log/scheduler） | GET /sse | 断线 2s 重连；`connState` 无消费者 → 后端死亡 UI 冻结无横幅 | B/C | L2+静态 |
| 3 | 菜单/设置页加载 schema | GET /schema/alas | 无 catch → 菜单/表单空白；**硬编码 CN 服选项** | A/C | 静态 |
| 4 | 设置页改参数（DynamicForm→saveValue） | POST /config/{name} | **invalid 列表被丢弃**：非法值静默跳回旧值；HTTP 错静默 | B | L2+静态 |
| 5 | 读取任意实例配置 | GET /config/{name} | **不存在的名字也返回 200 + 纯默认配置** | B/C | L2 |
| 6 | 导出配置 | GET /config/{name}/export | 裸 fetch 无 BASE、无 res.ok → 错误页可被存成 .json | B | 静态 |
| 7 | 导入配置 | POST /config/{name}/import | 覆盖同名实例无确认；可覆盖 template；响应不查 | B/C | 静态 |
| 8 | 实例列表 | GET /configs | 无 catch 空白；口径与 /status 不一致（mod 文件、backup 文件） | C | 静态 |
| 9 | 语言切换 | POST /language | 未知语言静默回落 en-US；**重启后不恢复** | B/E | L2 |
| 10 | 主题切换 | POST /theme | 任意字符串可持久化；**重启后不恢复** | E | L2 |
| 11 | 概览页 Start/Stop（主循环） | POST /run（无 func）/stop | **不查 ok、不 catch**：already running 被吞 | B | 静态 |
| 12 | 工具页 Start/Stop | POST /run（带 func）/stop | start 分支已查 ok（2f63e4af1 生效）；**stop 分支不查**；**alive 是实例级：主循环运行时按钮=Stop，点击杀主循环** | B/C | 静态 |
| 13 | 概览页调度三栏 | GET /scheduler/{name} | running=pending[:1] 伪 current，mount 时覆盖更准的 SSE 快照 | C | L2+静态 |
| 14 | 工具页 alive 判定 | GET /scheduler/{name} | 同上 alive 实例级 + **读路径内存级 override 使报告偏离磁盘** | C | L2 |
| 15 | 新建实例 | POST /instance/new | **无非法字符校验（rename 有）**：`x/y` 返回 ok:true 却写进 config/x/y.json，UI 凭空消失；重复名/空名有干净 ok:false | A/B | L2 |
| 16 | 重命名实例 | POST /instance/rename | 校验完整（字符/重名/template），正确 pop manager——**正面样板** | — | L2 |
| 17 | 删除实例 | POST /instance/delete | ok:false 干净；**但不清 _processes/_scheduler_cache → 幽灵 SSE 帧、旧日志复活** | E | L2+静态 |
| 18 | 检查更新 | GET /update/status | 形状与前端一致；git 模式 current 即版本戳 | — | L2 |
| 19 | 刷新 releases | POST /update/refresh | 立即 ok:true，失败仅日志；前端轮询器抛错**永久泄漏** | B | 静态 |
| 20 | 重启更新（git 模式） | POST /update/restart | 无 control_api 注入时降级 ok:false + 清晰文案——**正面样本** | — | L2 |
| 21 | 安装 release | POST /update/install | ok:false+error 处理正确；HTTP 错静默；轮询器同泄漏 | B | 静态 |
| 22 | 远程访问（Remote 页） | /remote/* | 页面"未支持"占位 + client.ts 三方法死代码；**start 未配置时 500 而非 ok:false** | A/C | L2 |
| 23 | 窗口控制（launcher tray） | /api/window* | headless 降级 ok:false+reason——**诚实，消费者是 tray 进程** | — | L2 |
| 24 | 静态资源 | / 与 /assets/* | no-cache 覆盖 text/html（L2 实测头）、assets 内容哈希；**全部 404（含缺失 asset）302 回 "/"** | D/C | L2+文件 |

死链：无（前端调用的路由全部存在）。孤儿：`/remote/*`（前端死代码 + 占位页）；`/api/window*` 的消费者是 launcher tray（设计如此，非孤儿）。形状不匹配：REST `/scheduler` vs SSE 快照两套形状（前端合并自洽于停止态，运行态为启发式）。

---

## b) 分类发现清单

### 高severity

**H1〔B〕POST /run 对无效 func / 不存在实例返回 ok:true，失败只存在于子进程日志**
- 证据：`module/webui/api/routers/control.py:24-30`（start 前只查 alive，无 func/实例白名单）；`module/webui/process_manager.py:184-186`（无匹配 → `logger.critical("No function matched")` 后进程以 Reason: Finish 退出）、`process_manager.py:113-119`（Finish → state=2"空闲"）。
- L2 实测：`POST /run {"func":"audit_nonexistent_probe"}` → `{"ok":true}`，3 秒后 state=2、alive=false，日志 `CRITICAL | No function matched: audit_nonexistent_probe`。前端拿到的唯一信号是按钮弹回 Start。
- 这是"点了启动没生效"的 API 级根源：2f63e4af1 修好了"传对 func"，但后端对"传错 func"仍然伪成功。
- 修法：/run 在 start 前校验 `func ∈ {"alas"} ∪ get_available_func() ∪ get_available_mod() ∪ get_available_mod_func()` 且 `instance ∈ alas_instance()`，否则 ok:false。

**H2〔C〕工具页 Start/Stop 的 alive 是实例级：主循环运行时进工具页显示"Stop"，点击会杀掉整个主循环**
- 证据：`webapp/src/views/Settings.svelte:43-48`（`toolAlive = sched.alive`，实例级 ProcessManager.alive）；`Settings.svelte:52-53`（`if (toolAlive) await api.stop(activeInstance)`）；`module/webui/api/routers/scheduler.py:16`。
- 影响：主循环运行中打开任意工具页（Daemon/Benchmark/…）→ 按钮显示 Stop → 点击 → 主循环被 `process.kill()`（`process_manager.py:74-77`），用户以为是"停这个工具"。反向：其他工具在跑时本页同样误显示。2f63e4af1 修复"传 func"后，Start(带 func) 分支在主循环运行期间根本不可达。
- 修法：用 SSE 快照 `schedulers[inst].current === selectedTask` 判定（或后端 /scheduler 增加 running_func），stop 前不一致则确认。

**H3〔E〕主题/语言"持久化"只写不读：每次重启强制回退 dark/zh-CN**
- 证据：`module/webui/setting.py:24`（`theme: str = "dark"` 硬编码）；`module/webui/lang.py:6`（`LANG = "zh-CN"` 硬编码）；全仓 `deploy_config.Theme/Language` 仅有两个写点（`lang.py:20`、`routers/theme.py:12`），**零读点**；启动钩子 `api/__init__.py:35-53` 只 `lang.reload()`（重建词典，不设 LANG）。
- L2 实测（v4 副本重启后）：deploy.yaml 已持久化 `Theme: default / Language: en-US`，`GET /status` 仍返回 `"theme":"dark","language":"zh-CN"`。
- 用户直接感知：改了语言/主题 → 重启 launcher → 全部回退，无任何提示。
- 修法：`_startup` 里 `State.theme = State.deploy_config.Theme`、`set_language(State.deploy_config.Language)`。

**H4〔C〕/scheduler 与 SSE recompute 的读路径做内存级 override：UI 调度栏报告偏离磁盘真值**
- 证据：`routers/scheduler.py:13-16` 与 `events.py:45-47` 构造一次性 `AzurLaneConfig` → `load()` → `config_override()`（=`override()`，`module/config/config.py:317-347`）：把 Commission/Research/Reward **在内存中强制启用**、把 NextRun > now+12h/24h 的任务**在内存中钳到 now**。`__init__`（config.py:93-100）不调 init_task，route/recompute 均不调 save() → **不写盘**（后端审原判"写回配置文件+与 bot 竞写"经实测否决，收窄为此条）。
- L2 实测：磁盘 `Main.Scheduler.NextRun=2026-10-31`、`Commission.Enable=false`，GET /scheduler 后磁盘不变，但响应把禁用任务照常列入 pending、时间按钳后值呈现。
- 影响：概览页"待办/等待"栏展示的是被内存 override 扭曲的调度状态，不是磁盘真值——用户看到"该跑了"而磁盘排期其实是另一回事。
- 修法：读路径只读不 override（或响应中标注来源），recompute 改纯读。

### 中severity

**M1〔B〕Home 概览 Start/Stop 不查 ok、不 catch**（`Home.svelte:50-57`）——"already running" 被吞、HTTP 错未处理 rejection，与 H2 叠加时 SSE 断线连点空转。修法：照 2f63e4af1 的 Settings 模式补齐。

**M2〔B/A〕保存接口的 `invalid` 被 UI 丢弃 + 272 个数值字段 schema 无 valuetype → 字符串腐坏静默通过**
- `Settings.svelte:72-80`（返回 `{valid,invalid}` 不读）；L2 实测：`ScreenshotInterval` 提交 `"abc"` → `{"valid":["Alas.Optimization.ScreenshotInterval"],"invalid":[]}`，磁盘存入字符串 `'abc'`。根因：`helpers.py:97` 仅对 select 做 option 类型推断，DynamicForm 对数字输入发送 `String(value)`（`DynamicForm.svelte:220`），而 args.json 中 **272 个**数字默认值的 input 字段无 valuetype（如 `Main.StopCondition.OilLimit`、`Main.Emotion.Fleet1Value`、各 `*.Scheduler.SuccessInterval`）。
- 修法：UI 展示 invalid；args 生成器为数字默认值补 valuetype（或 _save_config 对数字默认值做同样推断）。

**M3〔A/B〕/instance/new 缺非法字符校验：`x/y` 返回 ok:true 却写成 `config/x/y.json`，实例在 UI 凭空消失**
- `control.py:40-51`（无 `_INVALID_NAME_CHARS`，对比 rename `control.py:78-79`）+ `module/config/utils.py:60-64`（路径拼接）+ `deploy/atomic.py:141-148`（父目录不存在自动 makedirs）。L2 实测复现。`..` 与 Windows 盘符同源风险更高。前端 `Manage.svelte:22-32` 对 HTTP 500 无 catch，静默。
- 修法：new 复用 rename 校验；前端 createConfig 补 catch。

**M4〔E〕/instance/delete 不清理状态簿记：幽灵 SSE 帧、旧日志复活**
- `control.py:54-63` 不 pop `_processes`（rename `control.py:86` 有 pop——正确样板）；`events.py:19,94,118-136` 对残留 manager 每 30s 重算并推 SSE（已删配置读到全默认 → Commission/Research/Reward @2020-01-01 幽灵帧）、`_scheduler_cache` 新连接重放；同名重建后 `get_manager` 复用旧 manager → 旧日志重现。修法：delete 统一 pop + 清 `_scheduler_cache/_last_recompute`。

**M5〔B〕Develop 三个 update 轮询器：tick 抛错永不 clearInterval（泄漏+每 1.5s 未处理 rejection）、无卸载清理、可叠加**
- `Develop.svelte:43-53`（refreshReleases）、`84-91`（installRelease）；`65-75`（restartToUpdate）故意永等但卸载不清理。修法：tick 包 try/catch + 失败上限 + 卸载清理 + 入口防重入。

**M6〔B/C〕import 无覆盖确认/无名字与内容校验（可覆盖 template.json）；export 裸 fetch 无 res.ok**
- `Manage.svelte:38-67`、`routers/config.py:44-47`。同名实例被直接覆盖（delete 都有 confirm）；后端 import 不清 `Emulator.Serial`（new 有清，`control.py:49`）；export 漏 `VITE_API_BASE` 前缀 + 404 会被 302 跟随后把 index.html 存成 .json。修法：import 确认+校验+护 template+清 Serial；export 走 request()。

**M7〔B〕HTTP 异常统一裸奔**：`client.ts:13-15` 一律 throw，除 main.ts bootstrap 与 Develop.refreshUpdate 外全部调用点无 catch（Settings loadTask、AppMenu schema、Home refreshScheduler、Manage refresh/importFile、Develop 轮询器）→ 后端瞬断时各页面空白/静默。修法：request() 层统一错误上报或逐点 catch。

**M8〔A/C〕/schema 硬编码 CN 服**（`routers/schema.py:21` `server = to_server("cn")`）——非 CN 服实例的 select 选项展示 CN 集，选中值可能不在实例 server 的 `option_{server}` 内 → 读取时被 config_update 静默回落 option[0]（"config lost" 同类）。修法：按实例 PackageName 解析或前端传 server。

**M9〔A〕mod 实例（maa/fpy）的 webui 配置页读写错误文件**：`routers/config.py:40` 恒用 alas args 表；`helpers.py` 读写 `filepath_config(name)` 纯路径 = `./config/alas.json`，而 maa 实例真身是 `./config/alas.maa.json`（`module/submodule/utils.py:44-66`）→ **UI 改了等于没改**；/configs 又把它列成 "alas.maa"，两套口径。修法：config 路由按 `get_config_mod()` 分派，或禁用 mod 实例编辑。（代码级确认，本机无 maa 实例未实测）

**M10〔B/C〕GET /config/{不存在名字} 返回 200 + 纯默认配置；POST /config 到不存在的名字直接落盘造幽灵实例**
- `routers/config.py:30-33` + `config_updater.py:212-228`（缺失→{}→全默认）。L2 实测：`/config/nonexistent-audit` → 200 全量默认。与 L15 phantom 叠加：拼错实例名的整个编辑会话都是"真实感"的假象。修法：get/set 前校验 `config_name ∈ alas_instance()`。

**M11〔C〕SSE 连接状态 `connState` 写而不读**（`store.svelte.ts:13,67,98`；全仓零消费）→ 后端死亡后 UI 冻结：状态停更、按钮基于陈旧 alive 动作、REST 错误被 M7 吞掉——用户视角又是"点了没反应"。修法：AppHeader 依据 connState 显示离线横幅并禁用控制按钮。

**M12〔C〕REST /scheduler 的 `running` 实为 `pending[:1]`（到期队列头），Home.refreshScheduler 把它伪造成 `current` 展示在"Running"栏，并在每次 mount 覆盖更准的 SSE 快照**
- `routers/scheduler.py:17-22`、`Home.svelte:43-47`。优先级重排/配置修改后可错标运行任务。修法：仅在 SSE 快照为空时 bootstrap，且不从 running 造 current（或后端字段改名 `next`）。

### 低severity（摘要）

| # | 发现 | 位置 | 类 |
|---|---|---|---|
| L1 | /theme 无白名单；THEMES 三处漂移（App 6 值、theme.css 无 "light" 块只有 dark/minty/yeti/sketchy、Develop 只给 2 值、main.ts 无白名单） | theme.py:10-13; App.svelte:10; theme.css:142-295; Develop.svelte:24-27; main.ts:20 | A/C |
| L2 | /remote/start 双重伪成功：线程化立即 ok:true；未配置 SSHServer 时 500（ParseError 未接）而非 ok:false；SPA 端 remote* 三方法是死代码、Remote 页"未支持"硬编码 | remote.py:19-24; remote_access.py:160-164; client.ts:129-133; Develop.svelte:313-316 | A/C |
| L3 | 全部 404（含缺失的静态 asset）302 回 "/"：过期 shell 引用已删 asset 时拿回 HTML → MIME 白屏难诊断；API 拼路径得 HTML → request() 抛 SyntaxError | api/__init__.py:106-112 | C |
| L4 | PasswordGate：401 形态合规（`WWW-Authenticate: Basic realm="Alas"`，带凭据 200，L2 实测）；非 ASCII Authorization 头令 compare_digest 抛 TypeError→500（代码级）；WKWebView 对 Basic 401 自动重试待确认 | api/__init__.py:138-148 | B |
| L5 | `alas.backup.json` 类带点备份文件会被解析成 mod 实例：实例重名/口径不一致/主循环 RuntimeError（代码级） | submodule/utils.py:58-64; config/utils.py:153-171 | E |
| L6 | /stop 对任意名字惰性创建常驻 manager 条目（叠加 M4） | process_manager.py:121-128 | E |
| L7 | /run 不校验实例存在：拼错名 ok:true，以全默认配置跑主循环，首次 save 实体化幽灵实例 | control.py:25-30 | B |
| L8 | i18n 露键仅三条路径（未知 task query 标题、bootstrap 取词失败、词典永缓存）；前端 t() 兜底 zh-CN 与后端 /i18n 回落 en-US 不一致 | i18n.svelte.ts:20-21; i18n.py:11 | C/D |
| L9 | setLanguage/setTheme 忽略后端返回值（后端有归一化）乐观赋值 | Develop.svelte:93-103 | B |
| L10 | Settings.loadTask 快速切任务无竞态守卫（schema/config 可能错配） | Settings.svelte:26-41 | E |
| L11 | toolAlive 只在 mount/toggle 时查一次 REST，不随 SSE 更新：工具跑完后按钮仍"Stop"（点击无害 no-op 但语义错位） | Settings.svelte:43-48 | C |
| L12 | client.ts fetch 无超时/中止：后端挂起时 saving 永久 true | client.ts:11-17 | B |
| L13 | /update/refresh 立即 ok:true，失败仅 logger.exception | updater.py:19-28 | B |
| L14 | index.html `<html lang>`/`<title>` 硬编码；UI 无独立版本戳（D 类已由 no-cache+hash 资源+`/update/status`.current 三重缓解，见 §c.1） | index.html:2,8 | D |
| L15 | phantom 实例：config 目录无实例时 `alas_instance()` 兜底返回 'alas'——无文件的实例可被完整操作，配合 M10 在首次保存时实体化 | config/utils.py:171 | C |

### 正面确认（防误伤清单）

- **2f63e4af1 修复本身验证通过**：7 个工具任务 func（Daemon/OpsiDaemon/EventStory/IslandProductionPlanner/Benchmark/AzurLaneUncensored/GameManager）全部命中后端 dispatcher（后端审运行时验证 + `get_available_func()` 原文核对）；Settings start 分支查 ok 且错误上屏。
- import/export 往返逐字节一致（L2 实测）；rename 校验完整且有 pop（正确样板）；/api/window 降级诚实；/update/restart 无注入时降级文案清晰；PasswordGate 与 SSE 三事件形状（status/log/scheduler + keepalive 注释帧）实测正常。
- **D 类产物漂移当前不存在**：app bundle 与 repo 的 `webapp/dist` 全量哈希一致（assets/* + index.html）、关键后端模块一致（`__init__.py` 差异 = launcher control_api 注入补丁本体）、dist 不陈旧（无 src 文件新于 dist）、no-cache 头实测生效。

---

## c) L1/L2/L3 实测记录

### c.1 L1 实机只读 —— 红线冲突，改道文件级比对
任务红线禁止触碰 22267，而侦查（`lsof`：`python3.1 32782 TCP 127.0.0.1:22267 (LISTEN)`，父进程 alas-launcher 32727）证实**唯一在跑实例的 webui 端口就是 22267**（launcher 拉起 payload webui 并注入 control_api，同端口）。按"拿不准取保守"原则，未对真实例发任何请求（含 GET）。
替代验证（纯文件只读）：
- `/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript/webapp/dist` 与 repo `webapp/dist`：asset 列表与全部哈希**完全一致**（index.html sha256 `cacad76d…` 两边相同）；
- bundle `module/webui/api/routers/control.py`、`events.py`、`updater.py`、`process_manager.py`、`menu.json` 与 repo 一致；`api/__init__.py` 唯一差异 = `# === alas-launcher:control-api ===` 注入补丁（7 行，即运行实例 = repo HEAD + 注入）；
- L2 副本（同代码路径）实测响应头：`GET /` → `content-type: text/html; charset=utf-8` + `cache-control: no-cache` ✓；`GET /assets/index-*.js` → etag、无 no-cache（设计如此，内容哈希）✓。

### c.2 L2 隔离副本（核心实测）
环境：payload 整树 rsync 至 `/tmp/alas-audit-l2repo`（排除 .git/node_modules/log/.venv），原仓 `.venv/bin/python` 启动，`127.0.0.1:28319`，config 写入全部落在副本树（`iso-probe.json` 落点验证）；副本 deploy.yaml 硬化：`AdbExecutable=/nonexistent/audit-adb`、`ReplaceAdb/AutoConnect/InstallUiautomator2=false`（防失败路径波及宿主 adb server）。全程仅 localhost，未触任何真实设备（设备探活只发生在串行 `127.0.0.1:1` 上，connection refused）。
（过程事故与修复：第一版副本因 `module/logger.py:209` 的 `os.chdir` 强制把 CWD 归位仓库根，曾把 `audit-main.json`/`config/a/` 写进 repo——已即时停止、删除、`git status` 恢复干净（仅原有 `.DS_Store`），并改用整树复制方案重跑。repo 的 config/ 只有模板与 deploy.yaml，真实例数据在 app bundle 自身 config 内，无实际损害。）

关键实测原文（节选）：
```
POST /run {"instance":"iso-probe","func":"audit_nonexistent_probe"}
  → {"ok":true}                        # H1：预期之外的 ok:true
  → 3s 后 state=2；日志: CRITICAL | No function matched: audit_nonexistent_probe
POST /instance/new {"name":"x/y"}     → {"ok":true}；落盘 config/x/y.json，/status 不可见   # M3
POST /config/iso-probe {"Alas.Optimization.ScreenshotInterval":"abc"}
  → {"valid":[...],"invalid":[]}；磁盘存入 'abc'                                          # M2
GET /config/nonexistent-audit        → 200 + 全量默认配置                                  # M10
GET /config/{不存在}/export           → 200 attachment                                     # M6 关联
POST /remote/start                   → HTTP 500 Internal Server Error                     # L2
POST /language {"language":"xx-XX"}   → {"language":"en-US"}（静默回落，deploy.yaml 已写） # L9/H3
v4 重启后（deploy.yaml 已存 default/en-US）：GET /status → {"theme":"dark","language":"zh-CN"}  # H3 铁证
禁用 Commission 后 GET /scheduler：磁盘 Enable/NextRun 不变，响应仍列 pending             # H4（收窄证据）
Main.Scheduler.NextRun=2026-10-31 → GET /scheduler → 磁盘不变                            # 后端 F5"写盘"否决证据
POST /update/restart（无注入）        → {"ok":false,"error":"launcher integration not installed…"}  # 正面
rename "bad:name"                    → {"ok":false,"error":"Invalid characters in name: bad:name"} # 正面
import→export 往返                    → identical after import roundtrip: True              # 正面
/api/window                          → {"ok":false,"exists":false,…,"reason":"no desktop window…"}  # 正面
PasswordGate（v4，Password=audit-pass）：无凭据 401 + WWW-Authenticate；-u alas:audit-pass → 200   # L4
SSE：status/log/scheduler 帧形状与前端消费一致；keepalive 为注释帧（~25s 周期）
```

### c.3 L3 条件实弹 —— 红线冲突，探针在 L2 完成
真实例 = 22267 = 红线端口，L3 整段不适用（未对真实例发任何非 GET，也未发 GET）。设计中的 `audit_nonexistent_probe` 探针已在 L2 副本执行（§c.2），**原设计预期被实测修正**：预期 `ok:false 且错误干净`，实际 `ok:true` + 子进程日志 critical + state 回 2——这个修正本身就是 H1 的内容。

---

## d) 未验证清单

| 项 | 原因 |
|---|---|
| 真实例任何 HTTP 交互（含 L1 GET 与 L3 序列） | 端口=22267=红线端口，任务规则硬冲突；已用文件级比对 + L2 同代码路径替代 |
| dev 模式下 export 裸 fetch 缺 BASE 的实际表现（M6） | 未起 vite dev server（构建产物链路已由哈希比对覆盖） |
| WKWebView/Tauri 对 Basic 401 的自动重试行为（L4） | 需真机 webview 实验 |
| loadTask 竞态（L10）与轮询器泄漏（M5）的实机触发 | 需 UI 自动化；代码级成立 |
| webui _save_config 写盘与运行中 bot 保存的竞态窗口 | route 读路径确认不写盘（H4 收窄）；POST /config 与 bot 并发写仍是理论窗口（last-writer-wins），未实测 |
| M9 mod 实例错文件 | 本机无 maa/fpy 实例，代码级确认 |
| L5 backup 文件劫持、L4 非 ASCII 500 | 代码级确认，未构造运行时场景 |
| MaaEnd / AVD / 任何真实设备交互 | 全程红线禁入（副本串行恒为无效值 127.0.0.1:1，仅 connection refused） |

---

## e) 工具任务真机实测候选（仅列名与风险，未执行）

| 任务 | 风险备注 | 建议验证点 |
|---|---|---|
| Benchmark | 需设备；测量型任务，副作用小 | 日志出现 Benchmark 入口、SSE current 翻转、结束回 idle |
| EventStory | 依赖活动期入口；会实际操作游戏 UI | 活动期间人工窗口跑 |
| GameManager | 游戏管理操作（启动/关闭游戏） | 同上 |
| Daemon / OpsiDaemon | 长跑挂机任务，真跑=接管挂机；**勿在挂机时段** | 维护窗口短跑后 /stop |
| IslandProductionPlanner | 生产规划器（island_handler）；行为面未在本审查实测 | 先代码确认入口行为再排期 |
| AzurLaneUncensored | **高危：真改客户端文件**（反和谐） | 单独窗口 + 客户端备份 |

统一前置：维护窗口 + 实例空闲确认（/status state≠2 且日志二次确认）+ 每项跑完核对 /status 回 idle、SSE current 清空。

---

## 附录：双审分歧裁决记录

1. **前审 F10"IslandProductionPlanner 不在后端 dispatcher" → 否决**。原文核对：`module/submodule/utils.py:18-27` 的 `get_available_func()` 元组明确包含 `'IslandProductionPlanner'`；后端审独立运行时验证 7/7 工具 func 全部命中。该"发现"不成立。
2. **后审 F5"GET /scheduler 写回配置文件并与 bot 竞写" → 收窄改写为 H4**。裁决链：`AzurLaneConfig.__init__`（config.py:93-100）不调 `init_task`，route 与 SSE recompute 均不调 `save()`；L2 三次实测（远期 NextRun、禁用任务、phantom 配置）磁盘均未变。真实效应收缩为"内存级 override 使响应偏离磁盘"，不写盘、无竞写。

两次裁决都依赖"原文逐行抽查 + 隔离副本实测"而不是采信任一审查人的叙述——这正是本次审查针对"工具没生效"这类问题的方法论核心：**契约断裂与伪成功不会在单侧代码阅读中显形，只有跨端对账 + 真实请求/响应才可见**。

## 修复优先级建议

H1（/run 后端校验，堵伪成功通道）> H2（工具页 alive 语义）> H3（主题/语言回读，用户直接感知）> H4（调度栏真值）> M1/M7（错误呈现面）> M2（invalid+valuetype）> M3/M4（实例生命周期）> 其余。
