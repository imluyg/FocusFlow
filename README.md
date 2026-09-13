# FocusFlow 效率追踪器

FocusFlow 是一款 Windows 键鼠活跃度统计与效率分析工具：实时追踪键盘/鼠标/滚轮活动，
提供排行榜、趋势图、小时/星期分布等统计视图，并集成番茄钟、护眼休息、定时任务、
记账本等插件。

## 功能特性

- **实时统计**：今日活跃次数、当前速度（次/分），后台 500ms 刷新，UI 零阻塞
- **周期统计**：今日 / 7天 / 15天 / 30天 / 1年 / 总计，周期选择自动记忆（重启保持）
- **多视图**：键鼠排行、分组统计、趋势图（7/30 天）、小时分布、星期分布
- **悬浮窗**：透明置顶小窗实时显示活跃与速度，可拖动、双击打开主界面、位置记忆
- **系统托盘**：实时 tooltip（今日活跃/速度）、暂停时切换图标、左键呼出主界面
- **全局热键**：`Ctrl+Shift+F` 显示/隐藏主窗口，组合键与开关均可在设置中修改
  （**默认关闭**，需在设置页勾选"启用热键"后生效）
- **插件系统**（Lua）：番茄钟、定时任务、记账本、Edge 历史记录、统计概览
- **数据管理**：导入旧版数据、导出 CSV/HTML 报告、数据库压缩、自动备份轮转
- **外观**：液态玻璃风格（渐变背景 + 大圆角卡片），浅色/深色主题，Segoe UI 字体

## 项目结构

```
crates/
  core/      核心库：数据库、键鼠监听、统计、配置、Lua 插件宿主
  cli/       命令行工具：统计查询 / 导出 / 清空
desktop/     Tauri v2 桌面应用（当前主版本）
  src/       Rust 后端：命令、状态、托盘、热键、插件交互、导出
  ui/        前端（HTML/CSS/JS ES module，无构建步骤）
  capabilities/   Tauri 能力配置（ACL 权限）
  tauri.conf.json 窗口/打包配置
dist/       打包输出（绿色版 + NSIS 安装包，不入库）
```

## 构建

要求：Rust 1.80+、Node.js（仅 Tauri CLI 用 npx）、MSVC Build Tools、Windows 10/11。

```bash
# 绿色版（dist\FocusFlow\FocusFlow.exe）
build_tauri.bat

# NSIS 安装包（desktop/target/release/bundle/nsis/ 下）
desktop\build_nsis.bat
```

运行：直接双击 `dist\FocusFlow\FocusFlow.exe`，或安装 NSIS 安装包。
数据（键鼠记录、插件库）存放在程序目录 `data/`，备份在 `backup/`。
程序目录的定位优先级：`FOCUSFLOW_APP_DIR` 环境变量 > exe 所在目录（release 版）> 当前工作目录；
快捷方式的"起始位置"不影响数据落点。配置损坏时原文件会被备份为 `config.ini.corrupt-<时间戳>` 后重建。

## 开发

- 后端：`cargo build` / `cargo test --workspace` / `cargo clippy --workspace`；
  格式与 lint 在 CI（`.github/workflows/ci.yml`）强制执行。
- 前端：`desktop/ui/` 纯原生 ES module（无构建步骤）——`js/main.js` 为入口，
  主题变量唯一来源为 `tokens.css`。
- 插件：`crates/core/plugins/` 为源码，构建时复制到运行目录 `plugins/`（热重载）。

## 常见问题

- **启动后看不到主窗口**：默认启动进托盘，点击托盘图标或按全局热键呼出。
- **悬浮窗被任务栏遮挡**：程序每 500ms 重申置顶，最多 0.5 秒内自动浮回。
- **Edge 历史记录为 0**：Edge 运行时锁定数据库，程序自动复制副本读取；
  若仍为 0 请确认 Edge 安装路径为默认位置。
- **定时任务无法添加某个程序**：定时任务的目标受白名单限制（只放行常见用户应用，
  解释器与系统程序一律禁止，避免插件借此启动任意命令）。需要放行其它程序时，
  在 `config.ini` 的 `[scheduler]` 段加 `allow_extra = 你的程序.exe`。
- **导入旧数据不生效**：设置 → 数据操作 → 导入旧数据，选择旧版程序的 `data` 目录
  （导入结果会显示各年度条数与附属数据库）。

## 许可证

MIT
