// app_dir 串行锁审计：切全局程序目录的用例必须全程持有那把锁，也不许把它锚到 cwd。
//
//   node scripts/audit_app_dir_lock.mjs        # 违规则以退出码 1 结束并列出位置
//
// 为什么要有这一条：`set_app_dir` 写的是进程级 `Mutex<Option<PathBuf>>`，后写覆盖前者，
// 而测试并行跑在同一进程里。少了串行锁，症状不是"这条用例红"，而是隔壁用例的库被开到
// `target/ff_restore_sentinel/` 下、或者撞进仓库里的 `crates/core/data/` —— 随机红、
// 且 `--test-threads=1` 永远全绿。这类事不该靠人记得，所以写成机器判据。
//
// 第二条判据（CWD_PIN）查的是"锚到哪"：集成测试的 cwd 就是包目录 `crates/core`，
// `set_app_dir(current_dir())` 等于把**仓库**声明成程序目录 —— 宿主会在里面建 `data/`、
// `backup/`、`config.ini`，给下一条用例留旧行（`db/maintenance.rs` 里那条"实测本用例整套
// 跑时偶发 backup_dir 变成 crates/core"就是同一场事）。要读随包 `.lua` 就复制进临时目录
// （`crates/core/tests/common/mod.rs` 的 `isolated_app_dir`），仓库字节全程不动。
//
// 认三种"取了锁"的写法（都是本仓库里实际存在的形状）：
//   1) 直接调 `test_app_dir_lock()` / `app_dir_lock()`；
//   2) 调本文件里某个"函数体含 .lock()"的守卫（`guard()` / `isolate_app_dir()` 等，
//      这些 helper 常把 MutexGuard 塞进返回结构体里，靠字段声明序控制析构顺序）；
//   3) 自己是个 helper（返回 `TestAppDir` / 自建隔离结构），而**本文件里所有调用它的函数
//      都已经有守卫** —— 锁由调用方持有，helper 只是代为切目录。
import fs from "node:fs";
import path from "node:path";

const roots = process.argv.slice(2);
const dirs = roots.length ? roots : ["crates/core/src", "crates/core/tests", "desktop/src"];
const files = [];
(function walk(dir) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p);
    else if (e.name.endsWith(".rs")) files.push(p);
  }
})(dirs.length === 1 ? dirs[0] : dirs[0]);
for (const d of dirs.slice(1)) {
  (function walk(dir) {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.name.endsWith(".rs")) files.push(p);
    }
  })(d);
}

// 把源码切成「顶层/同缩进的 fn 块」：从 `^(\s*)fn NAME(...) {` 到同一缩进的 `^}` 为止。
const FN = /^([ \t]*)fn\s+([A-Za-z0-9_]+)[^{\n]*\{([\s\S]*?)^\1\}[ \t]*$/gm;

function parse(src) {
  const out = [];
  for (const m of src.matchAll(FN)) {
    out.push({
      name: m[2],
      // 签名一起留着：helper 的返回类型（`-> paths::TestAppDir`）写在括号与花括号之间，
      // 只看函数体会认不出它是"代为切目录的 helper"
      head: m[0].slice(0, m[0].indexOf("{")),
      body: m[3],
      line: src.slice(0, m.index).split("\n").length,
    });
  }
  return out;
}

let bad = 0;
let anchored = 0;
// 第二条判据的计数：`set_app_dir` 调用点总数（pins）与其中锚到 cwd 的条数（cwdBad）。
let pins = 0;
let cwdBad = 0;

// 剥掉注释再匹配：`tests/common/mod.rs` 的文件头就写着"旧写法于是直接
// `set_app_dir(current_dir())`"，不剥会红在一句说明上（本仓库在 presence 腿上进过同形亏）。
// 按行剥、保留行号。字符串里恰好含 `//` 的会被截短 —— 只会让判据更宽，不会假红。
const stripComments = (src) =>
  src
    .split("\n")
    .map((line) => {
      const t = line.trimStart();
      if (t.startsWith("//")) return "";
      const i = line.indexOf("//");
      return i >= 0 ? line.slice(0, i) : line;
    })
    .join("\n");

for (const f of files) {
  const src = fs.readFileSync(f, "utf8");
  const code = stripComments(src);
  const lineOf = (idx) => code.slice(0, idx).split("\n").length;
  // 谁把 cwd 存进了哪个变量：`let dir = std::env::current_dir()...`
  const cwdVars = new Set();
  for (const m of code.matchAll(/let\s+([A-Za-z_]\w*)\s*=\s*[\w:]*env::current_dir\s*\(/g)) {
    cwdVars.add(m[1]);
  }
  for (const m of code.matchAll(/set_app_dir\s*\(\s*([^)\n]*)\)/g)) {
    pins++;
    const arg = m[1].trim();
    const inline = arg.includes("current_dir(");
    const named = [...cwdVars].some((v) => new RegExp(`^&?\\s*\\b${v}\\b`).test(arg));
    if (inline || named) {
      cwdBad++;
      console.log(`CWD_PIN_VIOLATION ${f}:${lineOf(m.index)} 把 app_dir 锚到了 cwd(${arg})`);
    }
  }
  if (!src.includes("test_app_dir(")) continue;
  const fns = parse(src);
  // 第 1、2 类：自己取锁，或调了本文件里某个"已经在取锁"的函数。
  // 必须做到传递闭包：`isolate_app_dir()` 里写的是 `paths::test_app_dir_lock()`，
  // 函数体里并没有 `.lock(` 这个形状 —— 只按 `.lock()` 找守卫就会漏掉它，
  // 于是每条走 helper 的用例都被当成违规（第一版就是这么假红了 3 条）。
  const guardNames = new Set(["test_app_dir_lock", "app_dir_lock"]);
  const callsGuard = (body) =>
    [...guardNames].some((g) => new RegExp(`\\b${g}\\s*\\(`).test(body));
  for (let pass = 0; pass < 6; pass++) {
    let grew = false;
    for (const fn of fns) {
      if (guardNames.has(fn.name)) continue;
      if (/\.lock\s*\(/.test(fn.body) || callsGuard(fn.body)) {
        guardNames.add(fn.name);
        grew = true;
      }
    }
    if (!grew) break;
  }
  // 取了锁却没绑定 = 语句一结束就析构，等于没拿。这一条不查的话，
  // `paths::test_app_dir_lock();` 这样少写一个 `let _` 的笔误会一路绿灯。
  // 只盯"名字就像锁"的那些（两个公用的 + 本文件里 lock/guard/isolate 形状的 helper）：
  // 判据集合里还有一堆因为"函数体含 .lock()"而被算进守卫的普通函数，
  // 拿它们去查裸调用会假红 —— 第一版就在 `invalidate_years_cache();` 这一行上红过。
  const lockLike = (g) => /lock|guard|isolat/i.test(g);
  const discardsGuard = (body) =>
    [...guardNames].filter(lockLike).some((g) =>
      new RegExp("^[ \\t]*[\\w:]*" + g + "\\s*\\(\\s*\\)\\s*;[ \\t]*$", "m").test(body)
    );
  const holds = (body) => callsGuard(body) && !discardsGuard(body);

  const anchoredFns = fns.filter((fn) => fn.body.includes("test_app_dir("));
  anchored += anchoredFns.length;
  // 第 3 类：helper 豁免 —— 它自己没有守卫，但本文件里每个调用它的函数都有
  const callersOf = (name) =>
    fns.filter((fn) => fn.name !== name && new RegExp(`\\b${name}\\s*\\(`).test(fn.body));
  const exemptAsHelper = (fn) => {
    if (!/TestAppDir|Isolated/.test(fn.head + fn.body)) return false;
    const callers = callersOf(fn.name);
    return callers.length > 0 && callers.every((c) => holds(c.body));
  };

  for (const fn of anchoredFns) {
    if (holds(fn.body) || exemptAsHelper(fn)) continue;
    bad++;
    console.log(`VIOLATION ${f}:${fn.line}  ${fn.name}() 切了全局 app_dir 却没持有串行锁`);
  }
}
console.log(`APP_DIR_LOCK_AUDIT anchored=${anchored} violations=${bad}`);
console.log(`APP_DIR_CWD_PIN_AUDIT pins=${pins} violations=${cwdBad}`);
if (anchored < 100 || pins < 20) {
  // 扫到的量突然变少 = 解析器或目录清单坏了，那比漏报更危险（它会一直"绿"）
  console.log("AUDIT_BROKEN: 锚定函数或 set_app_dir 调用点少到不合理，判据本身大概失效了");
  process.exit(1);
}
process.exit(bad === 0 && cwdBad === 0 ? 0 : 1);
