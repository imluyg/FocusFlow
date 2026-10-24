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

# ---- 4b) app_dir 串行锁审计（切全局程序目录的用例必须持锁跑完）--------------------------
# 判据在 scripts/audit_app_dir_lock.mjs：违规则列位置并退非 0。
# 这条为什么归门禁而不是归人记：`set_app_dir` 写的是进程级 `Mutex<Option<PathBuf>>`，
# 后写覆盖前者；少一把锁的症状不是"这条用例红"，而是隔壁用例的库被开到哨兵目录、
# 或撞进仓库里的 crates/core/data —— 随机红且 `--test-threads=1` 永远全绿。
if command -v node >/dev/null 2>&1; then
    node scripts/audit_app_dir_lock.mjs > "$OUT/lock-audit.txt" 2>&1
    mark LOCK_AUDIT $?
    say "LOCK_AUDIT_DETAIL=$(tail -n 1 "$OUT/lock-audit.txt")"
else
    say "LOCK_AUDIT=node 不可用：这条判据不能跳过（它查的是测试纪律，不是环境）"
    mark LOCK_AUDIT 1
fi

# ---- 5) 真浏览器夹具与本机探针（存在即跑，跑了即致命）------------------------------------
# 表：脚本|成功标记。成功标记留空 ⇒ 用 RESULT::PASS/FAIL 计数判（PASS 必须 >0，防"整段没跑"）。
HARNESSES="
.scratch/run_tools_probe.sh|
.scratch/run_annot_mode_harness.sh|ANNOT_M1_OK
.scratch/run_pin_harness.sh|PIN_HARNESS_OK
.scratch/run_device_rename_harness.sh|DEVICE_RENAME_HARNESS_OK
.scratch/run_backup_hours_harness.sh|BACKUP_HOURS_HARNESS_OK
.scratch/run_modal_harness.sh|MODAL_HARNESS_OK
.scratch/run_chart_axis_harness.sh|CHART_AXIS_HARNESS_OK
.scratch/run_cleanup_harness.sh|CLEANUP_HARNESS_OK
.scratch/run_analysis_window_harness.sh|ANALYSIS_WINDOW_HARNESS_OK
.scratch/run_advanced_settings_harness.sh|ADVANCED_SETTINGS_HARNESS_OK
.scratch/run_export_period_harness.sh|EXPORT_PERIOD_HARNESS_OK
"
harness_ran=0
for entry in $HARNESSES; do
    script=${entry%%|*}
    token=${entry##*|}
    [ -f "$script" ] || { say "HARNESS_SKIP=$script"; continue; }
    name=$(basename "$script" .sh)
    log="$OUT/harness-$name.txt"
    sh "$script" > "$log" 2>&1
    code=$?
    harness_ran=$((harness_ran + 1))
    if [ -n "$token" ]; then
        # 判据是"日志最后一行恰好等于成功标记"，不是"标记出现了几次"：
        # runner 自己的 ABORT 文案里就带着这个 token（pin 那份日志里出现了 4 次），
        # 数次数既会误红也会在"中途打印过又失败退出"时误绿。末行相等才证明跑到了结尾。
        last=$(tail -n 1 "$log" | tr -d '\r\n')
        verdict=1
        [ "$code" = 0 ] && [ "$last" = "$token" ] && verdict=0
        say "HARNESS_$name=$([ "$verdict" = 0 ] && echo OK || echo RED)"
        [ "$verdict" != 0 ] && [ -z "$FIRST_FAILED" ] && FIRST_FAILED="HARNESS_$name"
        say "HARNESS_${name}_DETAIL=exit=$code last=[$last] 需要[$token]"
        mark "HARNESS_$name" "$verdict"
    else
        pass=$(grep -ac 'RESULT::PASS' "$log")
        fail=$(grep -ac 'RESULT::FAIL' "$log")
        verdict=$([ "$code" = 0 ] && [ "$fail" = 0 ] && [ "$pass" -gt 0 ] && echo 0 || echo 1)
        say "HARNESS_$name=$([ "$verdict" = 0 ] && echo OK || echo RED)"
        [ "$verdict" != 0 ] && [ -z "$FIRST_FAILED" ] && FIRST_FAILED="HARNESS_$name"
        say "HARNESS_${name}_DETAIL=exit=$code pass=$pass fail=$fail(要求 exit=0 fail=0 pass>0)"
        mark "HARNESS_$name" "$verdict"
    fi
done
say "HARNESSES_RAN=$harness_ran"

# ---- 6) tests/ui 转正后的浏览器套件（W4-3）------------------------------------------------
if [ -f tests/ui/run_all.mjs ]; then
    node tests/ui/run_all.mjs > "$OUT/ui.txt" 2>&1
    mark UI $?
    say "UI_SUITES=$(grep -ac '"status"' "$OUT/ui.txt")"
else
    say "UI_STEP=SKIP(tests/ui/run_all.mjs 不存在)"
fi

# ---- 7) 临时目录泄漏：跑完必须回到跑前状态 -------------------------------------------------
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
