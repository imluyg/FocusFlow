// app_dir 审计：切全局程序目录的用例必须全程持有那把锁，也不许把它锚到 cwd。
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
// 跑时偶发 backup_dir 变成 crates/core"就是同一场事）。要读的只是那几个 `.lua` 字节，
// 复制进临时目录即可（`crates/core/tests/common/mod.rs` 的 `isolated_app_dir`）。
//
// 认三种"取了锁"的写法（都是本仓库里实际存在的形状）：
//   1) 直接调 `test_app_dir_lock()` / `app_dir_lock()`；
//   2) 调本文件里某个"函数体含 .lock()"的守卫（`guard()` / `isolate_app_dir()` 等，
//      这些 helper 常把 MutexGuard 塞进返回结构体里，靠字段声明序控制析构顺序）；
//   3) 自己是个 helper（返回 `TestAppDir` / 自建隔离结构），而**每一个调用它的函数**
//      都已经有守卫 —— 锁由调用方持有，helper 只是代为切目录。
//
// "锚定"是传递的：函数体里出现 `test_app_dir(` 直接算，调用了锚定函数的也算（走 helper 的
// 用例换的是同一个进程级目录，锁照样得它自己持有）。跨文件认调用时要求那个 helper 的名字在
// 全仓只定义一次，否则 `setup()` 这类常见名会被别的文件里的同名函数顶班，把无关的调用者
// 拖进判据（假红）。
import fs from "node:fs";
import path from "node:path";

const roots = process.argv.slice(2);
const dirs = roots.length
  ? roots
  : ["crates/core/src", "crates/core/tests", "crates/cli/src", "desktop/src"];
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

// 剥掉注释再匹配：`tests/common/mod.rs` 的文件头原话就写着"旧写法于是直接
// `set_app_dir(current_dir())`"，不剥会红在一句说明上；而把被守卫的那句**注释掉**时，
// 不剥注释的守卫照样绿（本仓库在 presence 腿上进过这一亏）。
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

// 把源码切成「缩进 N 的 fn 块」：从 `^(\s*)(pub…)?fn NAME(…) {` 到同缩进的 `^}` 为止。
// 必须认 `pub fn` / `pub(crate) fn` / `pub(super) fn`：集成测试共用的夹具
// （`tests/common/mod.rs` 的 `isolated_app_dir`）与 `plugin_api_wiring.rs` 的
// `round7::guard()` 都带 `pub` —— 只认裸 `fn` 就把它们整条留在了判据视野外。
const FN =
  /^([ \t]*)(?:pub(?:\([\w: ]*\))?\s+)?fn\s+([A-Za-z0-9_]+)[^{\n]*\{([\s\S]*?)^\1\}[ \t]*$/gm;

function parse(file, code) {
  const out = [];
  for (const m of code.matchAll(FN)) {
    out.push({
      file,
      name: m[2],
      // 签名一起留着：helper 的返回类型（`-> paths::TestAppDir`）写在括号与花括号之间，
      // 只看函数体会认不出它是"代为切目录的 helper"
      head: m[0].slice(0, m[0].indexOf("{")),
      body: m[3],
      line: code.slice(0, m.index).split("\n").length,
    });
  }
  return out;
}

const sep = (k) => k.lastIndexOf("::");
const key = (file, name) => `${file}::${name}`;
const keyFile = (k) => k.slice(0, sep(k));
const keyName = (k) => k.slice(sep(k) + 2);

// 认"这个名字在这儿被调用了"：允许 `paths::foo()` / `crate::common::foo()`，
// 但**前面的 `.` 要否掉**。第一版没否它，五条假红全出在这个形状上：
// `tauri::Builder` 的 `.setup(move |app| …)` 被当成调用了 `tests/accounting_test.rs` 里
// 那个只定义过一次的 `setup()`，于是 `desktop/src/lib.rs` 的 `run()`、`main.rs` 的
// `main()`、`device_stats.rs` 的 `start_device_stats()` 都被拖成"锚定函数"，
// 而 `setup()` 自己又因为这些"跨文件调用方没取锁"被摘掉豁免。
const callRe = (name) => new RegExp(`(^|[^.\\w])${name}\\s*\\(`, "m");
const callsName = (body, name) => callRe(name).test(body);

// ---- 第一遍：逐文件解析，顺带跑第二条判据（锚到 cwd）--------------------------------------
let pins = 0;
let cwdBad = 0;
const perFile = [];
for (const f of files) {
  const code = stripComments(fs.readFileSync(f, "utf8"));
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
  perFile.push({ file: f, fns: parse(f, code) });
}

// 守卫闭包**按文件各建一份**：合并成一个全局集合的话，A 文件里那个不取锁的同名 `guard()`
// 会借到 B 文件 `guard()` 的信誉，白送一条豁免。
const fileGuards = new Map();
for (const p of perFile) {
  const guardNames = new Set(["test_app_dir_lock", "app_dir_lock"]);
  // 必须做到传递闭包：`isolate_app_dir()` 里写的是 `paths::test_app_dir_lock()`，
  // 函数体里并没有 `.lock(` 这个形状 —— 只按 `.lock()` 找守卫就会漏掉它，
  // 于是每条走 helper 的用例都被当成违规（第一版就是这么假红了 3 条）。
  const callsGuard = (body) => [...guardNames].some((g) => callsName(body, g));
  for (let pass = 0; pass < 8; pass++) {
    let grew = false;
    for (const fn of p.fns) {
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
  // 守卫集合里还有一堆因为"函数体含 .lock()"而被算进来的普通函数，
  // 拿它们去查裸调用会假红 —— 第一版就在 `invalidate_years_cache();` 这一行上红过。
  const lockLike = (g) => /lock|guard|isolat/i.test(g);
  const discardsGuard = (body) =>
    [...guardNames].filter(lockLike).some((g) =>
      new RegExp("^[ \\t]*[\\w:]*" + g + "\\s*\\(\\s*\\)\\s*;[ \\t]*$", "m").test(body)
    );
  fileGuards.set(p.file, { callsGuard, discardsGuard });
}
const holds = (fn) => {
  const g = fileGuards.get(fn.file);
  return g.callsGuard(fn.body) && !g.discardsGuard(fn.body);
};

// ---- 锚定集合：直接调 `test_app_dir(` 的算，调用锚定函数的传递着算 ------------------------
// 名字在全仓的定义次数：跨文件认调用时要求它唯一（见文件头那条防顶班）
const defCount = new Map();
for (const p of perFile) {
  for (const fn of p.fns) defCount.set(fn.name, (defCount.get(fn.name) || 0) + 1);
}
const visibleFrom = (calleeFile, calleeName, callerFile) =>
  calleeFile === callerFile || defCount.get(calleeName) === 1;
const anchored = new Set();
for (const p of perFile) {
  for (const fn of p.fns) {
    if (fn.body.includes("test_app_dir(")) anchored.add(key(p.file, fn.name));
  }
}
for (let pass = 0; pass < 8; pass++) {
  let grew = false;
  for (const p of perFile) {
    for (const fn of p.fns) {
      const k = key(p.file, fn.name);
      if (anchored.has(k)) continue;
      const callsAnchored = [...anchored].some((ak) => {
        const [af, an] = [keyFile(ak), keyName(ak)];
        return visibleFrom(af, an, p.file) && callsName(fn.body, an);
      });
      if (callsAnchored) {
        anchored.add(k);
        grew = true;
      }
    }
  }
  if (!grew) break;
}

// 第 3 类豁免：helper 自己没有守卫，但每一个调用它的函数都持有守卫（调用方跨文件找）。
const callersOf = (file, name) => {
  const out = [];
  for (const p of perFile) {
    for (const fn of p.fns) {
      if (fn.name === name && p.file === file) continue;
      if (!visibleFrom(file, name, p.file)) continue;
      if (callsName(fn.body, name)) out.push(fn);
    }
  }
  return out;
};
const exemptAsHelper = (fn) => {
  if (!/TestAppDir|Isolated/.test(fn.head + fn.body)) return false;
  const callers = callersOf(fn.file, fn.name);
  return callers.length > 0 && callers.every((c) => holds(c));
};

let bad = 0;
for (const k of anchored) {
  const p = perFile.find((x) => x.file === keyFile(k));
  const fn = p.fns.find((x) => x.name === keyName(k));
  if (holds(fn) || exemptAsHelper(fn)) continue;
  bad++;
  console.log(`VIOLATION ${p.file}:${fn.line}  ${fn.name}() 切了全局 app_dir 却没持有串行锁`);
}

console.log(`APP_DIR_LOCK_AUDIT anchored=${anchored.size} violations=${bad}`);
console.log(`APP_DIR_CWD_PIN_AUDIT pins=${pins} violations=${cwdBad}`);

// ---- 第三判据：串行化 app_dir 的锁全仓只能有一把 ------------------------------------------
// 上面那张守卫名单把 `app_dir_lock` 与 `test_app_dir_lock` **同等看待**，于是"各自另开一把"
// 这种形状在这条判据里是完全合规的 —— desktop 就真的自己 `static LOCK` 开过一把（注释写着
// "core 那把是 #[cfg(test)]，跨 crate 拿不到"，而它现在挂在 `test-utils` 特征上，拿得到）。
// 后果不是编译错误，而是两个用例组各自以为串行化了：它们并发跑，症状是随机红在
// `export::a_week_that_cannot_be_read…` 的反向腿与 `state::the_panel_is_told_which_readings_are_incomplete`
// 上（单跑绿、整套绿里夹红）。所以这里钉的是"调用形态"：只许一把自己建互斥体，其余必须委托。
//
// 范围说明：`crates/core/tests/*.rs` 各文件自己的 `test_lock()` / `guard()` 不被这条计入
// （名字不匹配），这是刻意的 —— 每个集成测试是**独立进程**，而 cargo 一次只跑一个测试
// 二进制，要串行化的是同一进程内的并行；实测那九个文件里没有混用两把锁的。
let lockDefiners = 0;
let lockDelegators = 0;
let lockBad = 0;
for (const p of perFile) {
  for (const fn of p.fns) {
    if (!/app_dir_lock$/.test(fn.name)) continue;
    const defines = /Mutex\s*::\s*new\s*\(/.test(fn.body);
    const delegates = callsName(fn.body, "test_app_dir_lock");
    if (defines && fn.name === "test_app_dir_lock") {
      lockDefiners++;
    } else if (delegates && !defines) {
      lockDelegators++;
    } else {
      lockBad++;
      console.log(
        `LOCK_SPLIT_VIOLATION ${p.file}:${fn.line}  ${fn.name}() 既不是那把唯一的锁，也没有委托给它`
      );
    }
  }
}
console.log(
  `APP_DIR_ONE_LOCK_AUDIT definers=${lockDefiners} delegators=${lockDelegators} violations=${lockBad}`
);
if (lockDefiners === 0) {
  // 一把定义都扫不到 = 解析器或名单坏了，而不是"纪律变好了"
  console.log("AUDIT_BROKEN: 没扫到任何定义 app_dir 串行锁的函数，判据本身大概失效了");
  process.exit(1);
}
if (anchored.size < 100 || pins < 20) {
  // 扫到的量突然变少 = 解析器或目录清单坏了，那比漏报更危险（它会一直"绿"）
  console.log("AUDIT_BROKEN: 锚定函数或 set_app_dir 调用点少到不合理，判据本身大概失效了");
  process.exit(1);
}
process.exit(bad === 0 && cwdBad === 0 && lockBad === 0 ? 0 : 1);
