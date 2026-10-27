# 验收脚本（acceptance harnesses）

这个目录是"改了代码之后，怎么在 5 分钟内知道有没有弄坏"的那套东西。
**全部 PowerShell，全部 ASCII-only**（Windows PowerShell 按 ANSI 读无 BOM 的 `.ps1`，
UTF-8 中文会把后面的 ASCII 字符吃掉，直接语法错误 —— 见各文件头注释）。

## 前提

- 已构建 release：`cargo build --release -p focusflow-native`（产物在**根** `target\release\`）；
- 跑安装包那条（`accept-installer.ps1`）还要先 `cmd /c build_native.bat`（`dist\*-setup.exe`）；
- ⚠ **想要"十二套读的是同一份字节"，就先 `build_native.bat` 再跑那十一套 GUI**：`build_native.bat` 第一步
  会重跑一遍 `cargo build --release`，实测它把 `target\release\FocusFlow.exe` 换成**另一份字节**
  （第十场：GUI 那十一套跑的是 `518D…`，打包之后变成 `F4DD…`，`setup.exe` 是 `CE8C…`）——
  源码一行没动也是两份产物，别说成"同一份"。
- 都需要 `desktop-native\probe\` 里的探针数据。
- 每个脚本都用 probe 的**副本**当 app 目录，不会碰真实数据。

## 怎么跑

```powershell
cd <repo>
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-smoke.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-floating.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-theme.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-collect.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-plugins.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-hotkey.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-datadir.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-plugsel.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-snip.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-report.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-pin.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-installer.ps1
```

每个脚本逐条打印 `PASS` / `FAIL`，最后一行 `---- N checks, M failed ----`，有失败时退出码 1。

## 各自验什么

| 脚本 | 检查点 | 现在的结果 |
|---|---|---|
| `accept-smoke.ps1` | 主窗出现 / 悬浮窗在 / `WM_NCHITTEST`=HTCAPTION / 窗口 1200×860 / 内存 ≤120 MB / PrintWindow 抓得到 / 验收状态行 / 无 error / 8 秒内退出 | 10 项；`-Collector` 时多验"采集开着" |
| `accept-floating.ps1` | 落点=配置 / 默认 104×48 / **搬动没落定时也要扛得住那一拍**（等过 1 秒重画周期再投 `WM_EXITSIZEMOVE`，两条腿：矩形还在 1300,300、配置也写回 1300,300）/ 重启位置还在 / 存 4000 被夹回 1900 / **整条落进任务栏带里必须被抬到工作区上方**（底边按卡片高度算，上/左/右仍按"露出 20 像素"）/ 双击唤回收起的主窗 | 13 项 |
| `accept-theme.ps1` | 深色 16 张 + 浅色 16 张逐视图截图 / 设置页「主题」行当场切换 / 行内 `EDIT` 背景色随主题（取主色，浅色 246,249,255、深色 31,40,59） | 10 项 |
| `accept-collect.ps1` | 点 5 下 → 今日 +5、速度 >0 / 暂停后不再涨、速度归 0 / 继续再 +5 / 退出 flush + 退出备份 / core 日志「数据库已关闭」 | 14 项 |
| `accept-plugins.ps1` | 插件视图：`button.modal` 开弹窗、弹窗与正文的 Select 写回、提交/关闭触发插件动作、Esc 关弹窗 | 11 项 |
| `accept-hotkey.ps1` | 两轮启动：配置里的三条组合按原样注册（日志逐字对）/ 一条写成裸键时程序照样起、日志点名被拒的那条、没被拒的那条照样注册 | 10 项 |
| `accept-datadir.ps1` | 两轮启动：`[paths]` 里那条待搬运标记由启动早期兑现（data/ 与 backup/ 真到了新根、旧根清空、标记归空、日志点名新根）/ 新根建不出来时数据原地不动、标记留着下次重试 | 13 项 |
| `accept-plugsel.ps1` | 自带一份 ASCII 小插件（分组表格 + `onselect` + `sel` 按钮）：Lua 里的 ids/group/onselect 真的走到宿主、选中写了声明的那个字段、`sel` 按钮拼出 `<按钮 id><记录 id>` | 10 项 |
| `accept-snip.ps1` | 用真注入的鼠标跑三条截图手势：拖框后快速版落 PNG / 标注版开窗不落盘、Esc 关掉；**单击选中光标下那个别人的窗口**（夹具是本脚本自己的一个 Form，产品必须排掉自己进程，选错了 PNG 尺寸就对不上）；**覆盖层挂满三分钟自己退**（这一条真等，见坑 7） | 21 项（含一条 180 秒的等待） |
| `accept-report.ps1` | 两轮启动：缺的那份上周周报由启动补上（文件名 `weekly-<上周一>.md`、有内容）/ 第二次启动看见已有文件就不动（时间戳不变） | 10 项 |
| `accept-pin.ps1` | 两轮：合成贴图开/收（`0x8000+12` 开 N 张、`0x8000+13` 全关）数得到窗口与尺寸、**退路一条条真按**（Esc 关单张 / 注入右键关单张 / Alt+F4 组合键关单张 / 全关后归零，另加"样式里没有标题栏⇒旧版那个 ✕ 在原生不存在"和"是 toolwindow⇒不进 Alt+Tab"两条读数）、**滚轮一格放大 10%（120×80→132×88）且连滚 20 格停在 1/4 而不是 0**、**贴图全程不多落一个 PNG**；第二轮走真手势（拖框 → 标注 → `P`）验**贴图正落在选区原位**、一次手势只写一个文件、贴到第 12 张为止且第 13 张被拒并说明原因，末尾打一行内存读数（不是判据） | 29 项 |
| `accept-installer.ps1` | 静默装 → 目录/卸载器/快捷方式/注册表/版本号/exe 图标 → **exe 自己带版本资源**（`FileVersion` 非空且与 setup.exe 同一个数，缺了就等于右键属性一片空白）→ 装出来的 exe 默认在采集 → 静默卸 → 全部清干净 | 19 项 |

> **跑之前先看有没有别的原生实例在跑**：`already_running()` 抢的是
> `Global\FocusFlow.Native.P0` 这个互斥体，抢不到就把已有窗口叫出来自己退出 ——
> 症状是每个脚本第一条 `main window appears` 就 FAIL、`focusflow.log` 一个字都没有。
> 现在这条不再需要人查：窗口等不到时脚本会先打一行
> `BLOCKER another instance holds the single-instance mutex: pid=… exe=…`，把占着的进程点名。
> 有就请先关掉那个实例（**别按名字杀**，旧版 WebView 也叫 `FocusFlow.exe`；
> 用 `Stop-App` 或按路径确认后再停）。
>
> 还有一条同源的坑：新版默认在采集，占着的实例会把脚本注入的点击记进**它自己的库**。
> 所以 GUI 套件要跑在"这台机器只有一个实例"的前提下 —— 这是产品行为，不是量具的毛病。

`accept-plugins.ps1` 断言的是应用日志里的 **ASCII 片段**（字段名 `d_type`/`f_cat`、动作 id
`add_modal`/`add_record`/`monthly_detail`/`clear_result`），不 OCR 截图；它把 `plugins/*.lua`
拷进 probe 副本，坐标按 100% DPI 的客户区算（与其它脚本同一假设）。

`accept-hotkey.ps1` 断言的也是日志里的 ASCII 片段（`toggle_window = ctrl+alt+7` 这类）：
注入的键盘到不了低级钩子（下面坑 1），按不出热键，所以它验的是「真正注册的那把 ==
配置与设置页显示的那把」，不是按下之后的效果。按下效果（含在设置页里改一格）要眼验。

`accept-plugsel.ps1` 不依赖任何记账数据：它自己往探针副本的 `plugins/` 里写一个小插件
（表格带 `ids`/`group`/`onselect`，外加一颗 `sel` 按钮），再用应用的
`FOCUSFLOW_PLUGIN_SEL` 钩子按**记录 id** 走一遍"点一行 + 按一颗 sel 按钮"——那条路径调的
就是真点击调的同两个方法。表格行的**像素坐标**随视图与滚动变化，注入点击不好算，所以坐标
那一段仍归眼验。

## 共享库

`accept-lib.ps1` 提供：`Check`/`Show-Summary`、`Get-DefaultExe`、`New-AppDir`（probe 副本 +
追加配置行）、`Start-App`（带 `FOCUSFLOW_APP_DIR` / `FOCUSFLOW_ACCEPTANCE=1`）、`Wait-Window`、
`Save-WindowShot`（`PW_RENDERFULLCONTENT`）、`Save-RectShot`、`Send-*`、`Get-Status`（解
`验收状态 today=… cpm=… collector=… paused=…`）、`Get-DominantColor`、`Set-ConfigLine`、
`Click-ViewTab` / `Click-ATab` / `Click-BasicRow` / `Click-PeriodPill`、`Get-InstalledDir`、
`Invoke-Setup`。

## 十个坑（都踩过）

1. **注入的键盘到不了低级键盘钩子**：`keybd_event`（带真实扫描码）与 `SendInput` 都试过，
   采集里一条都收不到；同一批注入的**鼠标**事件正常。所以采集脚本用点击计数，
   键盘这条由 core 的 `listener` 单测兜底。
2. **"今天"没数据时表格是空的**：设备行上的「改名」按钮也随之不存在。需要先点「7天」
   （`Click-PeriodPill 1`）再点行 —— `accept-theme.ps1` 就是这么做的。
3. **脚本必须 ASCII**：中文注释/字符串在 cmd 的 ANSI 读法下会被打散，症状是
   `Unexpected token` 或"字符串缺少结束符"。要匹配中文日志就用 `accept-lib.ps1` 里
   `Get-FailWord` 那种"按码点拼字符串"的写法。
4. **窗口出现 ≠ 悬浮窗就绪**：`FocusFlowNativeD2D` 在 `App::new` 之前就建好了，而悬浮窗是
   `App::new` 里才起的线程。所以找 pill 一定用 `Wait-Window ... 6000`，别用一次性的
   `ByClass`（会偶发 FAIL）。
5. **`WM_APP` 是 `0x8000`，`0x400` 是 `WM_USER`**：`accept-pin.ps1` 头一版按 `0x400+12` 投钩子，
   消息落进没人认领的号段，产品一声不响 ⇒ 五条红点看着像"贴图功能没做"，实际是量具投错了号。
   这类红要先问"消息到底有没有人接"，判据写在产品侧就照产品侧那个基数写（其它脚本用的
   `0x8008` / `0x8009` 是对的）。
6. **PowerShell 的 `-shl` 保持左操作数的类型**：对 `[byte]` 做 `1 -shl 8` 得到的是 0（在 8 位里绕回去），
   于是 `Get-PngSize` 把 306 宽的截图读成 50，红点看着像"截图截错了区域"。读大端整数要先
   `([uint32]$b[16] -shl 24)`——同一句里 `-bor` 而不是 `+`，避免进位串位。
7. **别处的 `WM_TIMER` 投不进这个循环**：实测给覆盖层投 `WM_TIMER`（wParam 用产品自己那个定时器 id，
   也试了 0）都没反应，而给同一个窗口投 `WM_KEYDOWN`/VK_ESCAPE 立刻关掉。所以"三分钟自动放弃"
   这类按时钟走的行为只能**真等**——`accept-snip.ps1` 那条就是等满 180 秒的，跑这套要多留三分钟内别抢鼠标。
   这条腿的敏感点是"**那 180 秒里不能有输入**"，跟单跑还是连跑无关：覆盖层是全屏幕的，那三分钟里
   任何一次真实点击都会走产品自己的落盘路径把它关掉（多一个 PNG、`snip-timeout` 那行不写），
   而前后各套用的都是自己的 `target\accept\<套件>\app`，截图目录互不相通，别处的残留进不了这条判据
   （`$before4` 本来就是本回合开始前的快照）。第八场把这条腿从"窗口没了"收紧成
   "窗口没了 **且** 确实等满了约 180 秒"——旧形状在实测那次红里是绿的（`waited 69 s` 照样 PASS），
   名字说的"自己退"它测不到。
8. **注入的滚轮跟着"前台窗口"走，不跟着光标走**：本机 `MouseWheelRouting=2`（悬停滚动是开着的），
   但三路对照量出来是：刚建好、焦点还在贴图上的那张 ⇒ 滚三格 `120×80→160×106`；同一张，先把主窗
   `SetForegroundWindow` 叫到前台再滚 ⇒ 尺寸一动不动；两张贴图里把焦点那张用 Esc 关掉、剩下一张没被
   别人抢过前台 ⇒ 照样滚得动。所以"光标停在你身上就该滚你"这件事**注入不出来**，`accept-pin.ps1`
   的滚轮腿先把那张贴图拿到前台、并先断言前台确实是它，控制点不成立时两条腿记红并写明"没跑成"而不是记绿。
   真实鼠标悬停滚动仍归眼验。另一个连带坑：贴图是**从左上角缩放**的，缩小时光标会被图"让出去"，
   所以要每滚一格重新量一次中心再投，一次投 20 格只会走到一半（实测停在 `63×42`）。
9. **"闸干净"不等于"这台机器空闲"**（第十场实测：`mutex=free`、`Get-Process` 数到 0，十一套连跑照样红八条，
   因为鼠标和前台在别人手里）。两种物证形状，红点自己会交代：
   ① 覆盖层那三分钟里**多落一张 PNG，尺寸就是身份证**——实测多出来的是 `194x117`（光标底下那个第三方窗），
   夹具那张选中是 `306x193`，全屏那张才是屏幕尺寸；它同时把"超时自己退"那三条腿打成红
   （`gone=True waited 19 s`、`snip-timeout` 那行不写）。这条腿在第八场从"窗口没了"收紧成
   "没了**且**等满约 180 秒"，就是为这一刻准备的：旧形状这次照样会绿。
   ② 注入 5 次点击读回 `today 0 -> 7`，而同一套里第二次注入精确 `7 -> 12` ⇒ 多出来的是别人的两下，
   不是"鼠标按下没去重"。
   `accept-pin` 的滚轮腿同理但**可复现**：这台机器 `HKCU\Control Panel\Desktop\ForegroundLockTimeout=200000`
   ⇒ 前台归别人时，后台进程的 `SetForegroundWindow` 被系统挡（单跑也一样），控制点不成立那三条就是
   "没测到"，不是缩放坏。⇒ **开跑前再量一行 `GetForegroundWindow()` 的 class/pid/proc**；
   不属于本工装（不是 `FocusFlow*` 类名、也不是自己的控制台）就别开跑，或开跑了也只许写"没测到"。
10. **复跑会把上一跑的物证抹掉**：十二套各有默认 WorkDir，`New-AppDir` 一开场就清空 `app\`，
    所以"为了归因红点而单跑复跑"会当场删掉上一跑的那张多落 PNG 与 `focusflow.log`（第十场就这么丢过一次
    —— 尺寸已在台账里，但工件不可再核）。
    ⇒ 复跑前先把 `<WorkDir>\focusflow.log`、`<WorkDir>\app\logs\` 和截图清单（**带像素尺寸**）抄走，
    或者给复跑传 `-WorkDir` 指一个新目录（十二套都有这个参数）。
