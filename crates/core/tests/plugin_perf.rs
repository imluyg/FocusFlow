//! 插件视图重建的性能基线（默认 `#[ignore]`，手动跑）。
//!
//! 目的：回答两个问题 ——
//! 1. 面板打开时每 1 秒一次的 `get_view()` 到底贵不贵；
//! 2. 开销落在 Lua 解释执行上，还是落在宿主侧的 Widget 解析与字符串拷贝上。
//!
//! 跑法：
//! ```text
//! cargo test -p focusflow-core --release --test plugin_perf -- --ignored --nocapture
//! ```
//!
//! 数字只用于**同机纵向对比**：换机器、换构建配置都会变，别当绝对指标引用。
//!
//! **对照臂怎么复现**（方案 3.9.1 那张三臂表里的"不复用"那一列）：这一份量具本身不带开关，
//! 它量的是当前构建的形状。要量"每次都重开一次库"那一臂，把 `db::connection::aux_conn` 里
//! 取池那一步改成永不命中即可（`AUX_POOL.with` 之后直接丢弃那份 `Option` 再往下走），
//! 改完重新跑这一条、再把那一行还原。**读数只在同一台机器、同一份释放构建上才有可比性**，
//! 且必须连着跑一次原状做复跑臂（A′）—— A 与 A′ 对不上说明这一台此刻不适合量东西。

mod common;

#[cfg(test)]
mod tests {
    use focusflow_core::config::FocusFlowConfig;
    use focusflow_core::db;
    use focusflow_core::plugins::manager::PluginManager;
    use std::time::{Duration, Instant};

    /// 串行锁：本文件要锚定进程级 `app_dir`。
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn pct(sorted: &[Duration], p: f64) -> Duration {
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let i = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[i]
    }

    #[test]
    #[ignore]
    fn bench_get_view() {
        let _guard = guard();
        let app = crate::common::isolated_app_dir("perf");
        let dir = app.path();

        let config: &'static FocusFlowConfig = Box::leak(Box::new(
            FocusFlowConfig::load(dir.join("config.ini")).expect("读配置失败"),
        ));
        let database = db::Database::init_readonly();
        let mut manager = PluginManager::new(config, database);

        let files = manager.discover_paths().expect("发现插件失败");
        for f in &files {
            if let Err(e) = manager.load_plugin(f, focusflow_core::plugins::PluginSource::Builtin) {
                println!("跳过 {}: {e}", f.display());
            }
        }

        // 两个合成样例，用来把「解释器/边界」和「业务查询」拆开：
        // - bench_empty：只返回一个 label，代表调用 + Widget 解析的底噪
        // - bench_table：纯 Lua 造 1000 行表，代表数据构造 + 跨边界拷贝
        let plugins_dir = dir.join("plugins");
        std::fs::write(
            plugins_dir.join("bench_empty.lua"),
            r#"
PLUGIN_NAME = "bench_empty"
function get_view()
  return { title = "空视图", widgets = { { type = "label", text = "x" } } }
end
"#,
        )
        .expect("写 bench_empty");
        std::fs::write(
            plugins_dir.join("bench_table.lua"),
            r#"
PLUGIN_NAME = "bench_table"
function get_view()
  local rows = {}
  for i = 1, 1000 do
    rows[i] = { "按键" .. i, tostring(i * 37) }
  end
  return {
    title = "千行表",
    widgets = {
      { type = "table", headers = { "键", "次数" }, rows = rows },
    },
  }
end
"#,
        )
        .expect("写 bench_table");
        for extra in ["bench_empty.lua", "bench_table.lua"] {
            let _ = manager.load_plugin(
                &plugins_dir.join(extra),
                focusflow_core::plugins::PluginSource::Builtin,
            );
        }

        println!("--- get_view 重建耗时（200 次，含首次调用后的稳定态）---");
        for name in [
            "bench_empty",
            "bench_table",
            "统计速览",
            "番茄工作法",
            "定时任务",
            "记账本",
        ] {
            if manager.get_plugin(name).is_none() {
                println!("{name}: 未加载，跳过");
                continue;
            }
            for _ in 0..5 {
                let _ = manager.refresh_view(name);
            }

            let mut samples = Vec::with_capacity(200);
            for _ in 0..200 {
                let t = Instant::now();
                if manager.refresh_view(name).is_err() {
                    println!("{name}: 刷新失败");
                    break;
                }
                samples.push(t.elapsed());
            }
            if samples.is_empty() {
                continue;
            }
            samples.sort();
            let sum: Duration = samples.iter().sum();
            println!(
                "{name}: n={} avg={:.2}ms p50={:.2}ms p95={:.2}ms max={:.2}ms",
                samples.len(),
                sum.as_secs_f64() * 1000.0 / samples.len() as f64,
                pct(&samples, 0.5).as_secs_f64() * 1000.0,
                pct(&samples, 0.95).as_secs_f64() * 1000.0,
                samples[samples.len() - 1].as_secs_f64() * 1000.0,
            );
        }
    }
}
