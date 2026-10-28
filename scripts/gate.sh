#!/bin/sh
# FocusFlow 仓库门禁的唯一实现（POSIX sh，Windows 上用 Git Bash 跑，或由 scripts/gate.ps1 包一层）。
#
#   sh scripts/gate.sh
#
# 记账规则：每一步的退出码单独写进 target/gate/last.log；任一步非 0 ⇒ 整体红，
# 末行打印 GATE_FAILED <第一个失败步>。日志同时打到 stdout。
# 夹具/探针脚本只在"文件存在"时才跑，且存在即致命 —— 不存在打 SKIP，绝不假装绿。

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd) || { echo "GATE_FAILED root-cd"; exit 1; }
cd "$ROOT" || { echo "GATE_FAILED root-cd"; exit 1; }

OUT=target/gate
LOG=$OUT/last.log
mkdir -p "$OUT" || { echo "GATE_FAILED cannot-create-target-gate"; exit 1; }
: > "$LOG"

say() { printf '%s\n' "$*" | tee -a "$LOG"; }

FIRST_FAILED=
mark() {
    code=$2
    [ -z "$FIRST_FAILED" ] && [ "$code" -ne 0 ] && FIRST_FAILED="$1"
    say "$1_EXIT=$code"
}

# /tmp 下本程序造的临时目录清单（名字形如 ff_<tag>_<pid>_<seq>）；数到"每种前缀各几个"，
# 一轮门禁跑完必须一条不差地回到跑前状态，否则就是临时目录泄漏。
temp_snap() {
    ls -d /tmp/ff_* 2>/dev/null | sed 's#.*/##; s/_[0-9]*_[0-9]*$//' | sort | uniq -c
}
temp_total() { ls -d /tmp/ff_* 2>/dev/null | wc -l | tr -d ' '; }
temp_snap > "$OUT/temp-before.txt"
say "TEMP_BEFORE_DIRS=$(temp_total)"

# ---- 0) 工具链自检：缺了直接红，不许"环境不在就算了" ------------------------------------
env_missing=0
for t in cargo rustc git; do
    if command -v "$t" >/dev/null 2>&1; then
        case $t in
            git) ver=$(git --version 2>/dev/null) ;;
            *)   ver=$("$t" -V 2>/dev/null) ;;
        esac
        say "ENV_OK=$t=$ver"
    else
        say "ENV_MISSING=$t"
        env_missing=1
    fi
done
if [ -f tests/ui/run_all.mjs ] && ! command -v node >/dev/null 2>&1; then
    say "ENV_MISSING=node（tests/ui/run_all.mjs 需要它）"
    env_missing=1
fi
mark ENV "$env_missing"

# ---- 1) 格式 ------------------------------------------------------------------------------
cargo fmt --all --check > "$OUT/fmt.txt" 2>&1
mark FMT $?

# ---- 2) clippy ----------------------------------------------------------------------------
cargo clippy --workspace --all-targets -- -D warnings > "$OUT/clippy.txt" 2>&1
mark CLIPPY $?

# ---- 3) 测试（--locked：Cargo.lock 不许被顺手改掉）----------------------------------------
cargo test --workspace --locked > "$OUT/test.txt" 2>&1
mark TEST $?
say "TEST_SUITES=$(grep -acE 'Running (unittests|tests)' "$OUT/test.txt")"
say "TEST_RESULT_OK=$(grep -ac 'test result: ok' "$OUT/test.txt")"
say "TEST_RESULT_FAILED=$(grep -ac 'test result: FAILED' "$OUT/test.txt")"
say "TEST_PASSED_TOTAL=$(grep -o 'test result: ok\. [0-9]*' "$OUT/test.txt" | awk '{s+=$4} END {print s+0}')"
# STATUS_ENTRYPOINT_NOT_FOUND 是本机换装/链接出问题时的一族症状，出现即红。
say "TEST_ENTRYPOINT_FAILURES=$(grep -ac 'STATUS_ENTRYPOINT_NOT_FOUND' "$OUT/test.txt")"
[ "$(grep -ac 'STATUS_ENTRYPOINT_NOT_FOUND' "$OUT/test.txt")" = "0" ]
mark TEST_ENTRYPOINT $?

# ---- 4) 文档（公开文档链的私有项只有 rustdoc 会红，clippy/test 全绿）----------------------
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps > "$OUT/doc.txt" 2>&1
mark DOC $?
# RUSTDOCFLAGS 拦不住依赖检查阶段吐的 rustc 警告，单独数一遍警告行数。
say "DOC_WARNING_LINES=$(grep -ac '^warning' "$OUT/doc.txt")"
[ "$(grep -ac '^warning' "$OUT/doc.txt")" = "0" ]
mark DOC_WARNINGS $?

# ---- 4b) app_dir 审计：切全局程序目录的用例必须持锁跑完，且不许锚到 cwd ------------------
# 判据在 scripts/audit_app_dir_lock.mjs：违规则列位置并退非 0。
# 这条为什么归门禁而不是归人记：`set_app_dir` 写的是进程级 `Mutex<Option<PathBuf>>`，
# 后写覆盖前者；少一把锁的症状不是"这条用例红"，而是隔壁用例的库被开到哨兵目录、
# 或撞进仓库里的 crates/core/data —— 随机红且 `--test-threads=1` 永远全绿。
# 锚到 cwd 是同一家族的另一半：集成测试的 cwd 就是包目录，那等于把仓库当程序目录用。
if command -v node >/dev/null 2>&1; then
    node scripts/audit_app_dir_lock.mjs > "$OUT/lock-audit.txt" 2>&1
    mark LOCK_AUDIT $?
    say "LOCK_AUDIT_DETAIL=$(grep -a '^APP_DIR_LOCK_AUDIT' "$OUT/lock-audit.txt")"
    say "CWD_PIN_AUDIT_DETAIL=$(grep -a '^APP_DIR_CWD_PIN_AUDIT' "$OUT/lock-audit.txt")"
    say "ONE_LOCK_AUDIT_DETAIL=$(grep -a '^APP_DIR_ONE_LOCK_AUDIT' "$OUT/lock-audit.txt")"
else
    say "LOCK_AUDIT=node 不可用：这条判据不能跳过（它查的是测试纪律，不是环境）"
    mark LOCK_AUDIT 1
fi

# ---- 5) 夹具引用完整性：路径写错要在跑之前叫出来，不许混进"判据没拿到" --------------------
# 判据在 scripts/check_harness_refs.mjs：把每份夹具里的 import / fetch / 标签引用按
# "服务器根 = 仓根、相对按夹具所在目录"解析成磁盘路径，逐个查在不在；不带 ./ 的写法
# 在模块图里是裸标识符，单独点出来。2026-10-29 那轮 14 条腿整片红就是这一族，
# 而当时唯一的读数是一句不区分成因的"一个判据都没拿到"。
if command -v node >/dev/null 2>&1; then
    node scripts/check_harness_refs.mjs > "$OUT/harness-refs.txt" 2>&1
    mark HARNESS_REFS $?
    say "HARNESS_REFS_DETAIL=$(grep -a '^HARNESS_REFS ' "$OUT/harness-refs.txt")"
    grep -a '^MISS \|^BARE ' "$OUT/harness-refs.txt" | tee -a "$LOG"
else
    say "HARNESS_REFS=node 不可用：这条查的是夹具纪律，不是环境，不许跳过"
    mark HARNESS_REFS 1
fi

# ---- 6) 真浏览器夹具与本机探针（存在即跑，跑了即致命）------------------------------------
# 表：脚本|成功标记|这一条证明什么。成功标记留空 ⇒ 用 RESULT::PASS/FAIL 计数判（PASS 必须 >0，
# 防"整段没跑"）。第三字段是 2026-10-29 补的口径：那 14 条里有 12 条断言的是**已删前端的归档
# 副本**（`.scratch/wv-ref/`），那份代码不会再变 ⇒ 它们红=副本坏/浏览器环境变，**不等于产品坏**；
# 只有 annot_mode 把 DOM 结果交给 cargo example 与 Rust 链路**对拍**，mem 那条量的是真产物。
# 绿的时候日志上就写着它属于哪一类，免得"14/14 OK"被读成"产品有 14 条浏览器覆盖"。
HARNESSES="
.scratch/run_tools_probe.sh||镜像(归档前端自证)
.scratch/run_annot_mode_harness.sh|ANNOT_M1_OK|对拍(JS↔cargo example)
.scratch/run_pin_harness.sh|PIN_HARNESS_OK|镜像(归档前端自证)
.scratch/run_device_rename_harness.sh|DEVICE_RENAME_HARNESS_OK|镜像(归档前端自证)
.scratch/run_backup_hours_harness.sh|BACKUP_HOURS_HARNESS_OK|镜像(归档前端自证)
.scratch/run_modal_harness.sh|MODAL_HARNESS_OK|镜像(归档前端自证)
.scratch/run_chart_axis_harness.sh|CHART_AXIS_HARNESS_OK|镜像(归档前端自证)
.scratch/run_cleanup_harness.sh|CLEANUP_HARNESS_OK|镜像(归档前端自证)
.scratch/run_analysis_window_harness.sh|ANALYSIS_WINDOW_HARNESS_OK|镜像(归档前端自证)
.scratch/run_advanced_settings_harness.sh|ADVANCED_SETTINGS_HARNESS_OK|镜像(归档前端自证)
.scratch/run_export_period_harness.sh|EXPORT_PERIOD_HARNESS_OK|镜像(归档前端自证)
.scratch/run_mem_harness.sh|MEM_SNAPSHOT_OK|本机(量真产物)
.scratch/run_floating_notice_harness.sh|FLOATING_NOTICE_HARNESS_OK|镜像(归档前端自证)
.scratch/run_floating_drag_harness.sh|FLOATING_DRAG_HARNESS_OK|镜像(归档前端自证)
"
harness_ran=0
harness_oracle_ok=0
harness_oracle_all=0
harness_local_ok=0
harness_local_all=0
harness_mirror_ok=0
harness_mirror_all=0
for entry in $HARNESSES; do
    script=${entry%%|*}
    rest=${entry#*|}
    token=${rest%%|*}
    kind=${rest##*|}
    [ -f "$script" ] || { say "HARNESS_SKIP=$script"; continue; }
    name=$(basename "$script" .sh)
    log="$OUT/harness-$name.txt"
    sh "$script" > "$log" 2>&1
    code=$?
    harness_ran=$((harness_ran + 1))
    case $kind in
        对拍*) harness_oracle_all=$((harness_oracle_all + 1)) ;;
        本机*) harness_local_all=$((harness_local_all + 1)) ;;
        *)     harness_mirror_all=$((harness_mirror_all + 1)) ;;
    esac
    if [ -n "$token" ]; then
        # 判据是"日志最后一行恰好等于成功标记"，不是"标记出现了几次"：
        # runner 自己的 ABORT 文案里就带着这个 token（pin 那份日志里出现了 4 次），
        # 数次数既会误红也会在"中途打印过又失败退出"时误绿。末行相等才证明跑到了结尾。
        last=$(tail -n 1 "$log" | tr -d '\r\n')
        verdict=1
        [ "$code" = 0 ] && [ "$last" = "$token" ] && verdict=0
        say "HARNESS_$name=$([ "$verdict" = 0 ] && echo OK || echo RED) [$kind]"
        [ "$verdict" != 0 ] && [ -z "$FIRST_FAILED" ] && FIRST_FAILED="HARNESS_$name"
        say "HARNESS_${name}_DETAIL=exit=$code last=[$last] 需要[$token]"
        mark "HARNESS_$name" "$verdict"
    else
        pass=$(grep -ac 'RESULT::PASS' "$log")
        fail=$(grep -ac 'RESULT::FAIL' "$log")
        verdict=$([ "$code" = 0 ] && [ "$fail" = 0 ] && [ "$pass" -gt 0 ] && echo 0 || echo 1)
        say "HARNESS_$name=$([ "$verdict" = 0 ] && echo OK || echo RED) [$kind]"
        [ "$verdict" != 0 ] && [ -z "$FIRST_FAILED" ] && FIRST_FAILED="HARNESS_$name"
        say "HARNESS_${name}_DETAIL=exit=$code pass=$pass fail=$fail(要求 exit=0 fail=0 pass>0)"
        mark "HARNESS_$name" "$verdict"
    fi
    if [ "$verdict" = 0 ]; then
        case $kind in
            对拍*) harness_oracle_ok=$((harness_oracle_ok + 1)) ;;
            本机*) harness_local_ok=$((harness_local_ok + 1)) ;;
            *)     harness_mirror_ok=$((harness_mirror_ok + 1)) ;;
        esac
    fi
done
say "HARNESSES_RAN=$harness_ran"
# 分类合计：产品口径只有"对拍 + 本机"这两类，其余是归档副本的规格镜像。
say "HARNESS_ORACLE_OK=$harness_oracle_ok/$harness_oracle_all (对拍)  HARNESS_LOCAL_OK=$harness_local_ok/$harness_local_all (本机)  HARNESS_MIRROR_OK=$harness_mirror_ok/$harness_mirror_all (镜像,不覆盖产品)"

# ---- 7) tests/ui 转正后的浏览器套件（W4-3）------------------------------------------------
if [ -f tests/ui/run_all.mjs ]; then
    node tests/ui/run_all.mjs > "$OUT/ui.txt" 2>&1
    mark UI $?
    say "UI_SUITES=$(grep -ac '"status"' "$OUT/ui.txt")"
else
    say "UI_STEP=SKIP(tests/ui/run_all.mjs 不存在)"
fi

# ---- 8) 临时目录泄漏：跑完必须回到跑前状态 -------------------------------------------------
temp_snap > "$OUT/temp-after.txt"
say "TEMP_AFTER_DIRS=$(temp_total)"
if diff "$OUT/temp-before.txt" "$OUT/temp-after.txt" > "$OUT/temp-diff.txt" 2>&1; then
    say "TEMP_FLAT=yes"
else
    say "TEMP_FLAT=no"
    cat "$OUT/temp-diff.txt" | tee -a "$LOG"
    mark TEMP_LEAK 1
fi

# ---- 汇总 ---------------------------------------------------------------------------------
if [ -z "$FIRST_FAILED" ]; then
    say "GATE_ALL_OK"
    exit 0
fi
say "GATE_FAILED=$FIRST_FAILED"
exit 1
