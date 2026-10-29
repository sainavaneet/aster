#!/usr/bin/env bash
# Orbit failure-injection demo: drives a throwaway `aster server` session through
# the assignment/report flow and its failure modes, printing `aster orbit activity`
# after each step. Exits non-zero if any expected state is not reached in time.
#
# Usage: cargo build && scripts/orbit_failure_demo.sh
#   ASTER_BIN=path/to/aster  binary to run (default: target/debug/aster)
#   ORBIT_DEMO_BUSY_SECONDS  how long the lead stays busy (default: 20)
#
# No real agent runs: Polaris and the workers are tests/fixtures/orbit/claude,
# a fake Claude that Aster detects as idle or working from its screen and that
# logs every prompt it receives. The session uses its own config home, socket,
# HOME, and a PATH without user directories, and is deleted on exit.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bin=${ASTER_BIN:-$repo/target/debug/aster}
busy_seconds=${ORBIT_DEMO_BUSY_SECONDS:-20}
timeout_seconds=40
[ -x "$bin" ] || { echo "error: $bin not found; run cargo build first" >&2; exit 1; }

base=$(mktemp -d /tmp/aster-orbit-demo.XXXXXX)
base=$(cd "$base" && pwd -P)
session="orbit-demo-$$"
state=$base/state
socket=$base/runtime/a.sock
mkdir -p "$base/config/aster-dev" "$base/runtime" "$state" "$base/work" "$base/bin" "$base/home"
cp "$repo/tests/fixtures/orbit/claude" "$base/bin/claude"
cat >"$base/config/aster-dev/config.toml" <<'EOF'
onboarding = false
[terminal]
default_shell = "/bin/sh"
shell_mode = "non_login"
[polaris]
model = "lead"
EOF

run_env() {
    env -i HOME="$base/home" TERM=xterm-256color SHELL=/bin/sh \
        PATH="$base/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
        XDG_CONFIG_HOME="$base/config" XDG_RUNTIME_DIR="$base/runtime" \
        ASTER_SOCKET_PATH="$socket" ASTER_SESSION="$session" ORBIT_FAKE_STATE="$state" "$@"
}
aster() { run_env "$bin" "$@"; }

server_pid=
start_server() {
    (cd "$base/work" && run_env ASTER_STARTUP_CWD="$base/work" "$bin" server \
        >>"$base/server.log" 2>&1 </dev/null) &
    server_pid=$!
    wait_for "Polaris lead idle at its prompt" lead_idle
}
stop_server() {
    [ -n "$server_pid" ] || return 0
    aster server stop >/dev/null 2>&1 || true
    local waited=0
    while kill -0 "$server_pid" 2>/dev/null && [ "$waited" -lt 150 ]; do
        sleep 0.1
        waited=$((waited + 1))
    done
    kill -9 "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=
}
cleanup() {
    status=$?
    stop_server
    rm -rf "$base"
    [ "$status" -eq 0 ] && echo "Demo passed." || echo "Demo FAILED (exit $status)." >&2
}
trap cleanup EXIT

fail() {
    echo "FAILED: $*" >&2
    echo "--- orbit activity ---" >&2
    aster orbit activity >&2 || true
    exit 1
}
wait_for() {
    local description=$1 deadline=$((SECONDS + timeout_seconds))
    shift
    until "$@"; do
        [ "$SECONDS" -lt "$deadline" ] || fail "timed out waiting for $description"
        sleep 0.25
    done
}
heading() { printf '\n==== %s ====\n' "$*"; }
show_activity() {
    echo "--- aster orbit activity ---"
    aster orbit activity | sed -n '/^Polaris lead/,/^Inspect output/p' | sed '$d'
}

lead_idle() {
    aster orbit activity 2>/dev/null | grep -q 'state=idle  visible_idle=yes'
}
lead_working() { aster orbit activity 2>/dev/null | grep -q 'state=working'; }
json_field() { grep -o "\"$1\":\"[^\"]*\"" | head -1 | cut -d'"' -f4; }
delivery() { aster orbit task-status "{\"task_id\":\"$1\"}" | json_field delivery; }
delivery_is() { [ "$(delivery "$1")" = "$2" ]; }
lead_prompts_with() { grep -cF "$1" "$state/lead.prompts" || true; }
lead_received() { [ "$(lead_prompts_with "$1")" -ge 1 ]; }

worker_ready() {
    local status
    status=$(aster orbit agent-status "{\"name\":\"$1\"}" 2>/dev/null) || return 1
    grep -q '"agent_status":"idle"' <<<"$status" && grep -q '"interactive_ready":true' <<<"$status"
}
start_worker() {
    local name=$1 pane=${2:-}
    local params="{\"name\":\"$name\",\"kind\":\"claude\",\"model\":\"$name\"}"
    [ -n "$pane" ] && params="{\"name\":\"$name\",\"kind\":\"claude\",\"model\":\"$name\",\"pane_id\":\"$pane\"}"
    aster orbit agent-start "$params" >/dev/null
    wait_for "worker $name idle" worker_ready "$name"
}

# assign <worker> <prompt>: sets task, worker_id, token from the assignment the
# worker's PTY delivered (the fake agent logs its input; its screen shows one line).
assign() {
    local output
    output=$(aster orbit agent-assign "{\"name\":\"$1\",\"prompt\":\"$2\"}") || fail "assign: $output"
    task=$(json_field task_id <<<"$output")
    wait_for "assignment $task at $1's PTY" report_command "$1" "$task"
    local line
    line=$(report_command "$1" "$task")
    worker_id=$(json_field worker_id <<<"$line")
    token=$(json_field ASTER_ORBIT_TOKEN <<<"$line")
    [ -n "$worker_id" ] && [ -n "$token" ] || fail "no report command for $task"
    echo "assigned $task to $1 (worker $worker_id)"
}
report_command() {
    grep -F '"json_argument"' "$state/$1.prompts" | grep -F "\"task_id\":\"$2\"" | tail -1 | grep .
}
# report <summary> [token]: the worker's report through the real CLI, with the
# token in ASTER_ORBIT_TOKEN as the assignment instructs.
report() {
    run_env ASTER_ORBIT_TOKEN="${2:-$token}" "$bin" orbit agent-report \
        "{\"task_id\":\"$task\",\"worker_id\":\"$worker_id\",\"outcome\":\"completed\",\"summary\":\"$1\"}" 2>&1
}
expect_report_ok() {
    local output
    output=$(report "$@") || fail "report refused: $output"
    echo "report accepted: $(json_field delivery <<<"$output")"
}
expect_report_error() {
    local expected=$1 output
    shift
    if output=$(report "$@"); then fail "report unexpectedly accepted: $output"; fi
    grep -qF "$expected" <<<"$output" || fail "expected \"$expected\", got: $output"
    echo "report rejected as expected: $expected"
}
marker() { printf '[orbit-delivery %s #1]' "$1"; }

heading "Setup: session $session in $base"
start_server
start_worker w1
show_activity

heading "1. Happy path: assign, worker reports via CLI, lead receives one delivery"
assign w1 "Summarise the README"
expect_report_ok "README summarised"
wait_for "delivery submitted" delivery_is "$task" submitted
wait_for "lead prompt" lead_received "$(marker "$task")"
show_activity

heading "2. Kill the worker mid-task: worker_lost and a server notice"
pane=$(aster orbit split '{"direction":"down"}' | json_field pane_id)
start_worker w2 "$pane"
assign w2 "Work that never finishes"
kill "$(cat "$state/w2.pid")"
echo "killed worker w2 (pid file $state/w2.pid)"
wait_for "worker_lost" delivery_is "$task" worker_lost
wait_for "server notice delivered" delivery_is "$task/server-notice" submitted
wait_for "notice on lead" lead_received "$(marker "$task/server-notice")"
show_activity

heading "3. Lead busy for ${busy_seconds}s: report deferred, then delivered"
assign w1 "Report while Polaris works"
echo busy >"$state/lead.mode"
wait_for "lead working" lead_working
expect_report_ok "deferred result"
deadline=$((SECONDS + busy_seconds))
while [ "$SECONDS" -lt "$deadline" ]; do
    current=$(delivery "$task")
    [ "$current" = queued ] || fail "delivery $current while the lead is busy"
    [ "$(lead_prompts_with "[orbit-delivery $task #")" -eq 0 ] || fail "typed into a busy lead"
    sleep 1
done
echo "still queued after ${busy_seconds}s; lead going idle"
show_activity
echo idle >"$state/lead.mode"
wait_for "deferred delivery" delivery_is "$task" submitted
show_activity

heading "4. Duplicate, conflicting, and bad-token reports"
assign w1 "Report more than once"
expect_report_error "Assignment not found or token invalid" "forged" "$(printf '0%.0s' {1..32})"
expect_report_ok "final answer"
wait_for "delivery" lead_received "$(marker "$task")"
expect_report_ok "final answer"
expect_report_error "Assignment already has a different report" "a different answer"
sleep 4.5
[ "$(lead_prompts_with "[orbit-delivery $task #")" -eq 1 ] || fail "lead received the report more than once"
echo "lead received exactly one delivery for $task"
show_activity

heading "5. Cancel: a later report is rejected"
assign w1 "Abandon this"
aster orbit task-cancel "{\"task_id\":\"$task\"}" | grep -q '"delivery":"cancelled"' || fail "cancel"
echo "cancelled $task"
expect_report_error "Assignment was cancelled by Polaris" "too late"
show_activity

heading "6. Restart mid-task: persisted ledger and terminals, then delivery"
assign w1 "Survive a restart"
stop_server
echo "server stopped; ledger: $(grep -c "$task" "$base/config/aster-dev/sessions/$session/orbit-tasks.json") entry for $task"
start_server
wait_for "worker w1 resumed" worker_ready w1
delivery_is "$task" awaiting_report || fail "task not restored as awaiting_report"
expect_report_ok "done after restart"
wait_for "delivery after restart" delivery_is "$task" submitted
wait_for "lead prompt after restart" lead_received "$(marker "$task")"
show_activity
