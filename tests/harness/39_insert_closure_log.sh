#!/usr/bin/env bash
# 39_insert_closure_log.sh — Batched closure promotion and stable view logs.
#
# Reproduces the real draft workflow: keep the draft checked out and promote
# its sequential changes into the parent. This pins both the metadata-only
# closure path and the rule that promotion cannot rewrite source log history.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  Suite: 39_insert_closure_log${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Promote a dependency closure while the draft remains checked out"

make_temp_repo "insert-closure-log"
init_repo

create_file "timeline.txt" "base\n"
assert_success "add baseline" atomic add timeline.txt
record_change "closure baseline" >/dev/null

assert_success "create draft from dev" new_view "feature" --draft --parent dev
assert_success "switch to draft" switch_view "feature"

append_file "timeline.txt" "first\n"
record_change "closure first" >/dev/null
append_file "timeline.txt" "second\n"
record_change "closure second" >/dev/null
append_file "timeline.txt" "third\n"
record_change "closure third" >/dev/null

assert_current_view "draft is checked out before promotion" "feature"

set +e
INSERT_OUT="$(ATOMIC_TRACE_INSERT=1 atomic insert 2>&1)"
INSERT_RC=$?
set -e

if [[ $INSERT_RC -eq 0 ]]; then
    _pass "bare insert promotes draft into parent"
else
    _fail "bare insert promotes draft into parent" "exit=$INSERT_RC output=$INSERT_OUT"
fi

if echo "$INSERT_OUT" | grep -qF '[insert_from_view] closure_plan complete'; then
    _pass "insert computes one complete closure plan"
else
    _fail "insert computes one complete closure plan" "trace did not contain closure_plan"
fi

if echo "$INSERT_OUT" | grep -qF '[insert_change]'; then
    _fail "ambient closure avoids per-change insertion" "trace entered insert_change"
else
    _pass "ambient closure avoids per-change insertion"
fi

assert_current_view "promotion leaves draft checked out" "feature"

FEATURE_LOG="$(atomic log 2>/dev/null || true)"
for message in "closure first" "closure second" "closure third"; do
    if echo "$FEATURE_LOG" | grep -qF "$message"; then
        _pass "draft log retains '$message' after promotion"
    else
        _fail "draft log retains '$message' after promotion" "record disappeared"
    fi
done
if echo "$FEATURE_LOG" | grep -qF "closure baseline"; then
    _fail "draft log contains only its stored records" "inherited baseline was copied into log"
else
    _pass "draft log contains only its stored records"
fi

DEV_LOG="$(atomic log --view dev 2>/dev/null || true)"
for message in "closure baseline" "closure first" "closure second" "closure third"; do
    if echo "$DEV_LOG" | grep -qF "$message"; then
        _pass "parent log contains '$message' after promotion"
    else
        _fail "parent log contains '$message' after promotion" "record missing"
    fi
done

REPEAT_OUT="$(atomic insert 2>&1 || true)"
if echo "$REPEAT_OUT" | grep -qiE 'nothing to insert|already even'; then
    _pass "repeated promotion is a no-op"
else
    _fail "repeated promotion is a no-op" "$REPEAT_OUT"
fi

print_summary
