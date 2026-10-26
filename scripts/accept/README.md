# 验收脚本（acceptance harnesses）

这个目录是"改了代码之后，怎么在 5 分钟内知道有没有弄坏"的那套东西。
**全部 PowerShell，全部 ASCII-only**（Windows PowerShell 按 ANSI 读无 BOM 的 `.ps1`，
UTF-8 中文会把后面的 ASCII 字符吃掉，直接语法错误 —— 见各文件头注释）。

## 前提

- 已构建 release：`cargo build --release -p focusflow-native`（产物在**根** `target\release\`）；
- 跑安装包那条（`accept-installer.ps1`）还要先 `cmd /c build_native.bat`（`dist\*-setup.exe`）；
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
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-installer.ps1
```

每个脚本逐条打印 `PASS` / `FAIL`，最后一行 `---- N checks, M failed ----`，有失败时退出码 1。

## 各自验什么

| 脚本 | 检查点 | 现在的结果 |
|---|---|---|
| `accept-smoke.ps1` | 主窗出现 / 悬浮窗在 / `WM_NCHITTEST`=HTCAPTION / 窗口 1200×860 / 内存 ≤120 MB / PrintWindow 抓得到 / 验收状态行 / 无 error / 8 秒内退出 | 10 项；`-Collector` 时多验"采集开着" |
| `accept-floating.ps1` | 落点=配置 / 默认 148×60 / 拖完写回 `pos_x,pos_y` / 重启位置还在 / 存 4000 被夹回 1900 / 双击唤回收起的主窗 | 11 项 |
| `accept-theme.ps1` | 深色 16 张 + 浅色 16 张逐视图截图 / 设置页「主题」行当场切换 / 行内 `EDIT` 背景色随主题（取主色，浅色 246,249,255、深色 31,40,59） | 10 项 |
| `accept-collect.ps1` | 点 5 下 → 今日 +5、速度 >0 / 暂停后不再涨、速度归 0 / 继续再 +5 / 退出 flush + 退出备份 / core 日志「数据库已关闭」 | 14 项 |
| `accept-plugins.ps1` | 插件视图：`button.modal` 开弹窗、弹窗与正文的 Select 写回、提交/关闭触发插件动作、Esc 关弹窗 | 11 项 |
| `accept-installer.ps1` | 静默装 → 目录/卸载器/快捷方式/注册表/版本号/exe 图标 → 装出来的 exe 能跑且采集默认关 → 静默卸 → 全部清干净 | 17 项 |

`accept-plugins.ps1` 断言的是应用日志里的 **ASCII 片段**（字段名 `d_type`/`f_cat`、动作 id
`add_modal`/`add_record`/`monthly_detail`/`clear_result`），不 OCR 截图；它把 `plugins/*.lua`
拷进 probe 副本，坐标按 100% DPI 的客户区算（与其它脚本同一假设）。

## 共享库

`accept-lib.ps1` 提供：`Check`/`Show-Summary`、`Get-DefaultExe`、`New-AppDir`（probe 副本 +
追加配置行）、`Start-App`（带 `FOCUSFLOW_APP_DIR` / `FOCUSFLOW_ACCEPTANCE=1`）、`Wait-Window`、
`Save-WindowShot`（`PW_RENDERFULLCONTENT`）、`Save-RectShot`、`Send-*`、`Get-Status`（解
`验收状态 today=… cpm=… collector=… paused=…`）、`Get-DominantColor`、`Set-ConfigLine`、
`Click-ViewTab` / `Click-ATab` / `Click-BasicRow` / `Click-PeriodPill`、`Get-InstalledDir`、
`Invoke-Setup`。

## 四个坑（都踩过）

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
