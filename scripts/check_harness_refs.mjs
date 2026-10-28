// 门禁用：把 .scratch 下浏览器夹具里的每一条资源引用**解析一遍并查文件在不在**。
//
//   node scripts/check_harness_refs.mjs
//
// 为什么归门禁而不是归人记：2026-10-29 那一轮 14 条夹具腿整片红，报的都是
// 「一个判据都没拿到（页面没加载完 / 模块 404）」——这句话不区分"路径写错"与"断言跑挂了"，
// 而真实成因是引用指向了随前端一起删掉的目录。修的过程里三种写法各错过一次：
//   1) `../desktop/ui/…`（相对，指向已经不存在的仓根目录）
//   2) `../wv-ref/…`：从 .scratch 里的页面出发多跳一层 ⇒ 落到服务器根
//   3) `wv-ref/…` 不带 `./` ⇒ 在 `<script type="module">` 里是**裸标识符**，浏览器直接拒绝解析
// 这三条在这里都是**跑之前就能算出来的事实**：解析出磁盘路径、看文件在不在、
// 再把裸标识符单独点出来。夹具目录不在（新克隆）时整步 SKIP，不假装绿。
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const root = path.resolve(process.cwd());
const dir = path.join(root, ".scratch");

if (!fs.existsSync(dir)) {
  console.log("HARNESS_REFS=SKIP(.scratch 不在)");
  console.log("HARNESS_REF_FIXTURES=0");
  process.exit(0);
}

const files = fs
  .readdirSync(dir)
  .filter((f) => f.endsWith(".html"))
  .map((f) => path.join(dir, f));

if (files.length === 0) {
  console.log("HARNESS_REFS=SKIP(没有夹具 HTML)");
  console.log("HARNESS_REF_FIXTURES=0");
  process.exit(0);
}

// JS 侧的取资源写法（import / 动态 import / fetch）：整份文件都要扫。
const JS_PATTERNS = [
  /(?:import|export)\s*(?:\{[^}]*\}|\*)?\s*from\s*["']([^"']+)["']/g,
  /import\s*\(\s*["']([^"']+)["']\s*\)/g,
  /fetch\s*\(\s*["'`]([^"'`]+)["'`]/g,
];
// 标记里的标签（<link href> / <script src>）：只数**真标记**。夹具会把标签当 `.replace()`
// 的搜索串写在 JS 字符串里（`const MODULE_TAG = '<script type="module" src="floating.js">'`），
// 那不是本文档的标签，也不该按本文档的基准去解析（三条悬浮窗夹具先被这样误判过）。
// 判据取"紧邻 `<` 的前一个字符"：是引号就说明这处标签活在字符串里。
// 不用跟踪"是否在 <script> 内"——那些夹具自己就把 `</script>` 拼成 `"<" + "/script>"`，
// 按行开关会被它自己打乱（第一版就是这么误判的）。
const MARKUP_PATTERNS = [
  /<link[^>]+href\s*=\s*["']([^"']+)["']/gi,
  /<script[^>]+src\s*=\s*["']([^"']+)["']/gi,
];

const inStringTag = (line, index) => {
  let i = index - 1;
  while (i >= 0 && (line[i] === " " || line[i] === "\t")) i -= 1;
  return i >= 0 && (line[i] === "'" || line[i] === '"' || line[i] === "`");
};

let checked = 0;
let missing = 0;
let bare = 0;
const problems = [];

for (const file of files) {
  const text = fs.readFileSync(file, "utf8");
  const lines = text.split(/\r?\n/);

  const checkOne = (spec, where) => {
    // 外链、数据 URI、纯片段、带模板插值的（拼出来的串这里查不了）都跳过。
    if (/^(https?:|data:|blob:|#)/i.test(spec)) return;
    if (spec.includes("${")) return;
    checked += 1;
    if (!/^\.{1,2}\//.test(spec) && !spec.startsWith("/")) {
      bare += 1;
      problems.push(`BARE  ${where}  ${spec}  ← 模块图里这是裸标识符，浏览器不会解析`);
      return;
    }
    // 绝对路径按"服务器根 = 仓根"算；相对路径按夹具自己所在目录算。
    const target = spec.startsWith("/")
      ? path.join(root, spec)
      : path.resolve(path.dirname(file), spec);
    if (!fs.existsSync(target)) {
      missing += 1;
      problems.push(`MISS  ${where}  ${spec}  →  ${path.relative(root, target)}`);
    }
  };

  lines.forEach((line, idx) => {
    const where = `${path.basename(file)}:${idx + 1}`;
    for (const re of JS_PATTERNS) {
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(line)) !== null) checkOne(m[1].trim(), where);
    }
    for (const re of MARKUP_PATTERNS) {
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(line)) !== null) {
        if (inStringTag(line, m.index)) continue;
        checkOne(m[1].trim(), where);
      }
    }
  });
}

for (const p of problems) console.log(p);
console.log(`HARNESS_REF_FIXTURES=${files.length}`);
console.log(`HARNESS_REFS checked=${checked} missing=${missing} bare=${bare}`);
process.exit(missing + bare > 0 ? 1 : 0);
