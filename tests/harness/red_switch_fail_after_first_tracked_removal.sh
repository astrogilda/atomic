#!/usr/bin/env bash
# Red regression contract for interruption-safe bridge switching.
#
# This filename intentionally has no numeric prefix, so run_all.sh excludes it.
# The current implementation is expected to fail these desired-future assertions.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo ""
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"
echo "${BOLD}  RED: bridge switch recovers after partial tracked removal${RESET}"
echo "${BOLD}══════════════════════════════════════════════════════════════${RESET}"

begin_section "Prerequisites"
require_git

atomic_current_view() {
    atomic view list 2>/dev/null | awk '/^\*/ { print $2; exit }'
}

checkpoint_hash() {
    shasum -a 256 .atomic/bridge/workspace.json | awk '{ print $1 }'
}

snapshot_complete_state() {
    local destination="$1"
    : > "$destination"
    find . \( -path './.git' -o -path './.atomic' \) -prune -o -print |
        LC_ALL=C sort |
        while IFS= read -r path; do
            [[ "$path" == "." ]] && continue
            if [[ -L "$path" ]]; then
                printf 'link\t%s\t%s\n' "${path#./}" "$(readlink "$path")"
            elif [[ -d "$path" ]]; then
                printf 'dir\t%s\n' "${path#./}"
            elif [[ -f "$path" ]]; then
                printf 'file\t%s\t' "${path#./}"
                shasum -a 256 "$path" | awk '{ print $1 }'
            else
                printf 'other\t%s\n' "${path#./}"
            fi
        done > "$destination"
}

assert_equal() {
    local label="$1"
    local expected="$2"
    local actual="$3"
    if [[ "$actual" == "$expected" ]]; then
        _pass "$label"
    else
        _fail "$label" "expected '${expected}', got '${actual}'"
    fi
}

assert_clean_statuses() {
    local label="$1"
    local git_status atomic_status
    git_status="$(git status --short)"
    atomic_status="$(atomic status --short 2>/dev/null || true)"
    if [[ -z "$git_status" ]]; then
        _pass "$label: Git status is clean"
    else
        _fail "$label: Git status is clean" "$git_status"
    fi
    if [[ -z "$atomic_status" ]]; then
        _pass "$label: Atomic status is clean"
    else
        _fail "$label: Atomic status is clean" "$atomic_status"
    fi
}

begin_section "Create offline main and feature projections"
make_temp_repo "red-switch-partial-removal"
init_git_repo
create_file "shared.txt" "shared projection\n"
create_file "target-only.txt" "main target-only content\n"
git add shared.txt target-only.txt
git commit --quiet -m "Main projection"
MAIN="$(git_current_branch)"

assert_success "import main into Atomic without network or vault" atomic git import --no-vault
assert_success "create feature view" atomic view create feature --draft --parent "$MAIN"
assert_success "align feature view and Git branch" atomic view switch feature --force

# Two lexically sorted source-only tracked paths guarantee that the failpoint
# after the first tracked removal observes a genuinely partial materialization.
rm target-only.txt
create_file "source-only/01-first.txt" "first source-only tracked file\n"
create_file "source-only/02-second.txt" "second source-only tracked file\n"
git add -A
git commit --quiet -m "Feature projection with two source-only files"
assert_success "project feature through the experimental bridge" atomic git bridge reconcile
assert_clean_statuses "source projection before interrupted switch"
assert_success "source projection verifies" atomic git bridge verify

SOURCE_HEAD="$(git_head_sha_full)"
SOURCE_BRANCH="$(git_current_branch)"
SOURCE_VIEW="$(atomic_current_view)"
SOURCE_CHECKPOINT="$(checkpoint_hash)"
SOURCE_STATE="$(mktemp "${TMPDIR:-/tmp}/atomic-red-switch-source-XXXXXX")"
AFTER_FAILURE_STATE="$(mktemp "${TMPDIR:-/tmp}/atomic-red-switch-failed-XXXXXX")"
_HARNESS_TMPDIRS+=("$SOURCE_STATE" "$AFTER_FAILURE_STATE")
snapshot_complete_state "$SOURCE_STATE"

begin_section "Interrupt after the first sorted tracked removal"
set +e
FAIL_OUTPUT="$(ATOMIC_FAIL_SWITCH_AFTER_FIRST_TRACKED_REMOVAL=1 atomic git bridge switch "$MAIN" 2>&1)"
FAIL_RC=$?
set -e
if [[ "$FAIL_RC" -ne 0 ]]; then
    _pass "failpoint makes bridge switch fail"
else
    _fail "failpoint makes bridge switch fail" "command unexpectedly succeeded: $FAIL_OUTPUT"
fi

ACTUAL_HEAD="$(git_head_sha_full)"
ACTUAL_BRANCH="$(git_current_branch)"
ACTUAL_VIEW="$(atomic_current_view)"
ACTUAL_CHECKPOINT="$(checkpoint_hash)"
ACTUAL_GIT_STATUS="$(git status --short)"
ACTUAL_ATOMIC_STATUS="$(atomic status --short 2>/dev/null || true)"
snapshot_complete_state "$AFTER_FAILURE_STATE"

begin_section "Desired future rollback contract"
assert_equal "source Git HEAD is restored" "$SOURCE_HEAD" "$ACTUAL_HEAD"
assert_equal "source Git branch is restored" "$SOURCE_BRANCH" "$ACTUAL_BRANCH"
assert_equal "source Atomic view is restored" "$SOURCE_VIEW" "$ACTUAL_VIEW"
assert_equal "bridge checkpoint is unchanged" "$SOURCE_CHECKPOINT" "$ACTUAL_CHECKPOINT"
if cmp -s "$SOURCE_STATE" "$AFTER_FAILURE_STATE"; then
    _pass "all source paths, types, and contents are restored with no target-only files"
else
    _fail "all source paths, types, and contents are restored with no target-only files" \
        "$(diff -u "$SOURCE_STATE" "$AFTER_FAILURE_STATE" || true)"
fi

begin_section "Desired future automatic recovery on ordinary retry"
set +e
RETRY_OUTPUT="$(atomic git bridge switch "$MAIN" 2>&1)"
RETRY_RC=$?
set -e
if [[ "$RETRY_RC" -eq 0 ]]; then
    _pass "ordinary retry automatically recovers and succeeds"
else
    _fail "ordinary retry automatically recovers and succeeds" "exit $RETRY_RC: $RETRY_OUTPUT"
fi
assert_equal "retry aligns Git branch to target" "$MAIN" "$(git_current_branch)"
assert_equal "retry aligns Atomic view to target" "$MAIN" "$(atomic_current_view)"
assert_clean_statuses "target projection after retry"
assert_success "bridge verify succeeds after retry" atomic git bridge verify

if [[ "$TESTS_FAILED" -gt 0 ]]; then
    echo ""
    echo "${BOLD}${RED}EXPECTED RED:${RESET} current bridge switch does not roll back or journal a mid-materialization failure."
    echo "${RED}  Actual state immediately after failpoint:${RESET}"
    echo "    Git:    branch=${ACTUAL_BRANCH:-<detached>} head=$ACTUAL_HEAD"
    echo "    Atomic: view=${ACTUAL_VIEW:-<unknown>}"
    echo "    Checkpoint: $ACTUAL_CHECKPOINT"
    echo "    Git status:"
    printf '%s\n' "${ACTUAL_GIT_STATUS:-<clean>}" | sed 's/^/      /'
    echo "    Atomic status:"
    printf '%s\n' "${ACTUAL_ATOMIC_STATUS:-<clean>}" | sed 's/^/      /'
    echo "${RED}  This is the known target-pointer/mixed-worktree state; assertions intentionally remain the desired future contract.${RESET}"
fi

print_summary
