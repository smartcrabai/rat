#!/usr/bin/env bats

setup() {
  local repo_root
  repo_root="$(cd "$BATS_TEST_DIRNAME/.." && pwd)"
  RAT_BIN="${RAT_BIN:-$repo_root/target/debug/rat}"
  if [[ "$RAT_BIN" != /* ]]; then RAT_BIN="$repo_root/$RAT_BIN"; fi
  export RAT_BIN

  TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/rat-e2e.XXXXXX")"
  TEST_CWD="$TEST_ROOT/cwd"
  mkdir -p "$TEST_CWD" "$TEST_ROOT/home"
  export RAT_HOME="$TEST_ROOT/home"
  export TZ=UTC
  cd "$TEST_CWD"

  RAT_DAEMON_PID=""
  RAT_OWNED_CHILD_PID=""
  RAT_LAST_RUN_ID=""
}

teardown() {
  stop_daemon
  cleanup_command_child
  rm -rf "$TEST_ROOT"
}

start_daemon() {
  "$RAT_BIN" daemon >"$TEST_ROOT/daemon.log" 2>&1 &
  RAT_DAEMON_PID=$!

  local attempt output
  for attempt in {1..100}; do
    if output=$("$RAT_BIN" add 86400 -- ': rat-e2e-readiness' 2>&1); then
      return 0
    fi
    if ! kill -0 "$RAT_DAEMON_PID" 2>/dev/null; then
      printf 'daemon exited before becoming available: %s\n' "$output" >&2
      return 1
    fi
    sleep 0.1
  done
  printf 'daemon did not become available before the 10s deadline: %s\n' "$output" >&2
  return 1
}

stop_daemon() {
  if [[ -n "$RAT_DAEMON_PID" ]]; then
    if kill -0 "$RAT_DAEMON_PID" 2>/dev/null; then
      kill -TERM "$RAT_DAEMON_PID" 2>/dev/null || true
      local attempt
      for attempt in {1..50}; do
        if ! kill -0 "$RAT_DAEMON_PID" 2>/dev/null; then break; fi
        sleep 0.1
      done
      kill -KILL "$RAT_DAEMON_PID" 2>/dev/null || true
    fi
    wait "$RAT_DAEMON_PID" 2>/dev/null || true
    RAT_DAEMON_PID=""
  fi
}

cleanup_command_child() {
  if [[ -z "$RAT_OWNED_CHILD_PID" && -f "$TEST_ROOT/child.pid" ]]; then
    RAT_OWNED_CHILD_PID="$(cat "$TEST_ROOT/child.pid")"
  fi
  case "$RAT_OWNED_CHILD_PID" in
    ''|*[!0-9]*) return ;;
  esac
  kill -TERM "$RAT_OWNED_CHILD_PID" 2>/dev/null || true
  sleep 0.1
  kill -KILL "$RAT_OWNED_CHILD_PID" 2>/dev/null || true
  RAT_OWNED_CHILD_PID=""
}

json_rows() {
  python3 -c 'import json,sys; d=json.load(sys.stdin); rows=d if isinstance(d,list) else next((d[k] for k in ("jobs","runs","items","data") if isinstance(d,dict) and isinstance(d.get(k),list)), None); assert isinstance(rows,list), "expected a JSON array or an object containing jobs/runs/items/data"; print(len(rows))'
}

job_count() {
  "$RAT_BIN" list --all --json | json_rows
}

job_id_for_command() {
  "$RAT_BIN" list --all --json | python3 -c 'import json,sys; d=json.load(sys.stdin); rows=d if isinstance(d,list) else next((d[k] for k in ("jobs","items","data") if isinstance(d,dict) and isinstance(d.get(k),list)), []); wanted=sys.argv[1]; matches=[]
for j in rows:
 definition=j.get("definition", {}) if isinstance(j,dict) else {}
 command=j.get("command", definition.get("command")) if isinstance(j,dict) else None
 if command == wanted: matches.append(j)
assert len(matches)==1, f"expected one job with command {wanted!r}, found {len(matches)}"
print(matches[0]["id"])' "$1"
}

job_field() {
  "$RAT_BIN" show "$1" --json | python3 -c 'import json,sys; d=json.load(sys.stdin); j=d.get("job",d) if isinstance(d,dict) else d[0]; field=sys.argv[1]
def walk(x):
 if isinstance(x,dict):
  for k,v in x.items():
   if k==field: yield v
   yield from walk(v)
 elif isinstance(x,list):
  for v in x: yield from walk(v)
values=list(walk(j))
if field=="command":
 definition=j.get("definition",{}) if isinstance(j,dict) else {}
 value=j.get("command",definition.get("command")) if isinstance(j,dict) else None
else:
 value=values[0] if values else None
if isinstance(value,(dict,list)): print(json.dumps(value,sort_keys=True))
elif value is not None: print(value)' "$2"
}

history_field() {
  "$RAT_BIN" history "$1" --json | python3 -c 'import json,sys; d=json.load(sys.stdin); rows=d if isinstance(d,list) else next((d[k] for k in ("runs","items","data") if isinstance(d,dict) and isinstance(d.get(k),list)), []); field=sys.argv[1];
def norm(v): return str(v).lower().replace("_","").replace("-","")
if not rows: print("")
else:
 r=rows[-1]
 value=r.get(field)
 print(norm(value) if field=="status" else ("" if value is None else value))' "$2"
}

history_run_id() {
  "$RAT_BIN" history "$1" --json | python3 -c 'import json,sys; d=json.load(sys.stdin); rows=d if isinstance(d,list) else next((d[k] for k in ("runs","items","data") if isinstance(d,dict) and isinstance(d.get(k),list)), []); assert rows, "expected run history"; print(rows[-1]["id"])'
}

history_count() {
  "$RAT_BIN" history "$1" --json | python3 -c 'import json,sys; d=json.load(sys.stdin); rows=d if isinstance(d,list) else next((d[k] for k in ("runs","items","data") if isinstance(d,dict) and isinstance(d.get(k),list)), None); assert isinstance(rows,list); print(len(rows))'
}

wait_for_history_status() {
  local job_id="$1" expected="$2" attempt current
  for attempt in {1..100}; do
    current="$(history_field "$job_id" status 2>/dev/null || true)"
    if [[ "$current" == "$expected" ]]; then
      RAT_LAST_RUN_ID="$(history_run_id "$job_id")"
      return 0
    fi
    sleep 0.1
  done
  printf 'job %s did not reach run status %s before the 10s deadline (last: %s)\n' "$job_id" "$expected" "$current" >&2
  return 1
}

wait_for_job_state() {
  local job_id="$1" expected="$2" attempt current
  for attempt in {1..100}; do
    current="$(job_field "$job_id" state 2>/dev/null | tr '[:upper:]' '[:lower:]' | tr -d '_-' || true)"
    if [[ "$current" == "$expected" ]]; then return 0; fi
    sleep 0.1
  done
  printf 'job %s did not reach state %s before the 10s deadline (last: %s)\n' "$job_id" "$expected" "$current" >&2
  return 1
}

wait_for_file() {
  local path="$1" attempt
  for attempt in {1..100}; do
    [[ -s "$path" ]] && return 0
    sleep 0.1
  done
  printf 'file %s was not created before the 10s deadline\n' "$path" >&2
  return 1
}

line_count() {
  python3 -c 'import pathlib,sys; p=pathlib.Path(sys.argv[1]); print(len(p.read_text().splitlines()) if p.exists() else 0)' "$1"
}

@test "offline add gives manual-daemon guidance and registers nothing" {
  run "$RAT_BIN" add 3600 -- 'printf should-not-register'
  [ "$status" -ne 0 ]
  [[ "$output" == *"rat daemon"* || "$output" == *"rat service install"* ]]
  [ "$(job_count)" -eq 0 ]
}

@test "add accepts exactly one command-string argument after --" {
  start_daemon
  local before
  before="$(job_count)"

  run "$RAT_BIN" add 3600 --
  [ "$status" -ne 0 ]
  run "$RAT_BIN" add 3600 -- 'printf first' 'printf second'
  [ "$status" -ne 0 ]
  [ "$(job_count)" -eq "$before" ]
}

@test "invalid and overflowing one-off schedules are rejected without registration" {
  start_daemon
  local before
  before="$(job_count)"

  run "$RAT_BIN" add 25:99 -- 'true'
  [ "$status" -ne 0 ]
  run "$RAT_BIN" add -1 -- 'true'
  [ "$status" -ne 0 ]
  run "$RAT_BIN" add 999999999999999999999999999999999999 -- 'true'
  [ "$status" -ne 0 ]
  run "$RAT_BIN" add 20260230T120000Z -- 'true'
  [ "$status" -ne 0 ]
  [ "$(job_count)" -eq "$before" ]
}

@test "zero is immediate and relative add reports a saved absolute time" {
  start_daemon
  local zero_marker="$TEST_ROOT/zero.marker" relative_command relative_id next_at
  run "$RAT_BIN" add 0 -- "printf zero >> '$zero_marker'"
  [ "$status" -eq 0 ]
  [[ "$output" == *"UTC"* ]]
  python3 -c 'import re,sys; assert re.search(r"20\d{2}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}",sys.stdin.read()), "add output must include an absolute date-time"' <<<"$output"
  local zero_id
  zero_id="$(job_id_for_command "printf zero >> '$zero_marker'")"
  wait_for_file "$zero_marker"
  wait_for_history_status "$zero_id" succeeded

  relative_command='true'
  run "$RAT_BIN" add 15 -- "$relative_command"
  [ "$status" -eq 0 ]
  [[ "$output" == *"UTC"* ]]
  relative_id="$(job_id_for_command "$relative_command")"
  next_at="$(job_field "$relative_id" next_at)"
  python3 -c 'from datetime import datetime,timezone; import sys; value=datetime.fromisoformat(sys.argv[1].replace("Z","+00:00")); now=datetime.now(timezone.utc); assert value.tzinfo is not None; delta=(value-now).total_seconds(); assert 10 <= delta <= 16, f"expected a saved absolute time about 15s ahead, got {delta}s"' "$next_at"
}

@test "JSON list/show, run history and output reflect real command effects" {
  start_daemon
  local marker_command job_id run_id listing shown history output_text
  marker_command="printf stdout-token; printf stderr-token >&2"
  run "$RAT_BIN" add 5 -- "$marker_command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$marker_command")"

  listing="$("$RAT_BIN" list --json)"
  printf '%s' "$listing" | python3 -c 'import json,sys; d=json.load(sys.stdin); rows=d if isinstance(d,list) else d.get("jobs",d.get("items",[])); assert any(str(j.get("id"))==sys.argv[1] for j in rows)' "$job_id"
  shown="$("$RAT_BIN" show "$job_id" --json)"
  printf '%s' "$shown" | python3 -c 'import json,sys; d=json.load(sys.stdin); j=d.get("job",d); assert str(j.get("id"))==sys.argv[1]; definition=j.get("definition",{}); assert j.get("command",definition.get("command"))==sys.argv[2]' "$job_id" "$marker_command"

  wait_for_history_status "$job_id" succeeded
  run_id="$RAT_LAST_RUN_ID"
  history="$("$RAT_BIN" history "$job_id" --json)"
  printf '%s' "$history" | python3 -c 'import json,sys; d=json.load(sys.stdin); rows=d if isinstance(d,list) else d.get("runs",d.get("items",[])); assert rows and str(rows[-1].get("id"))==sys.argv[1]; assert str(rows[-1].get("status")).lower().replace("_","")=="succeeded"' "$run_id"
  output_text="$("$RAT_BIN" output "$run_id" 2>&1)"
  [[ "$output_text" == *"stdout-token"* ]]
  [[ "$output_text" == *"stderr-token"* ]]
  [[ "$output_text" == *"stdout"* ]]
  [[ "$output_text" == *"stderr"* ]]
}

@test "overdue paused one-off requires explicit confirmation before catch-up" {
  start_daemon
  local marker="$TEST_ROOT/resume.marker" command job_id
  command="printf resumed >> '$marker'"
  run "$RAT_BIN" add 2 -- "$command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$command")"
  run "$RAT_BIN" pause "$job_id"
  [ "$status" -eq 0 ]
  sleep 2.2
  [ ! -e "$marker" ]

  run "$RAT_BIN" resume "$job_id"
  [ "$status" -ne 0 ]
  [ ! -e "$marker" ]
  run "$RAT_BIN" resume "$job_id" --confirm-overdue
  [ "$status" -eq 0 ]
  wait_for_file "$marker"
  [ "$(line_count "$marker")" -eq 1 ]
}

@test "editing a future command changes the eventual execution" {
  start_daemon
  local old_command new_command marker job_id
  marker="$TEST_ROOT/edited.marker"
  old_command='printf old-command'
  new_command="printf edited-command >> '$marker'"
  run "$RAT_BIN" add 5 -- "$old_command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$old_command")"
  run "$RAT_BIN" edit "$job_id" -- "$new_command"
  [ "$status" -eq 0 ]
  [ "$(job_field "$job_id" command)" = "$new_command" ]
  wait_for_file "$marker"
  [ "$(line_count "$marker")" -eq 1 ]
  wait_for_history_status "$job_id" succeeded
}

@test "editing a running one-off keeps it consumed and leaves daemon dispatching" {
  start_daemon
  local started="$TEST_ROOT/held.started" release="$TEST_ROOT/release" original_marker="$TEST_ROOT/original.marker"
  local edited_marker="$TEST_ROOT/edited.marker" independent_marker="$TEST_ROOT/independent.marker"
  local command edited_command independent_command job_id independent_id
  command="printf started > '$started'; printf '%s' \"\$\$\" > '$TEST_ROOT/child.pid'; attempt=0; while [ ! -e '$release' ] && [ \"\$attempt\" -lt 100 ]; do attempt=\$((attempt + 1)); sleep 0.1; done; [ -e '$release' ] || exit 124; printf original >> '$original_marker'"
  run "$RAT_BIN" add 0 -- "$command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$command")"
  wait_for_file "$started"
  wait_for_file "$TEST_ROOT/child.pid"
  RAT_OWNED_CHILD_PID="$(cat "$TEST_ROOT/child.pid")"
  case "$RAT_OWNED_CHILD_PID" in ''|*[!0-9]*) printf 'command wrote an invalid child PID\n' >&2; return 1 ;; esac
  wait_for_history_status "$job_id" running

  edited_command="printf edited > '$edited_marker'"
  run "$RAT_BIN" edit "$job_id" -- "$edited_command"
  [ "$status" -eq 0 ]
  [[ "$output" == *"no future occurrence"* ]]
  [ -z "$(job_field "$job_id" next_at)" ]

  run "$RAT_BIN" edit "$job_id" --timezone Pacific/Auckland
  [ "$status" -eq 0 ]
  [[ "$output" == *"no future occurrence"* ]]
  [ -z "$(job_field "$job_id" next_at)" ]
  [ "$(job_field "$job_id" timezone)" = "Pacific/Auckland" ]

  independent_command="printf independent > '$independent_marker'"
  run "$RAT_BIN" add 0 -- "$independent_command"
  [ "$status" -eq 0 ]
  independent_id="$(job_id_for_command "$independent_command")"
  wait_for_file "$independent_marker"
  wait_for_history_status "$independent_id" succeeded

  printf released >"$release"
  wait_for_history_status "$job_id" succeeded
  [ "$(cat "$original_marker")" = original ]
  [ ! -e "$edited_marker" ]
  [ "$(job_field "$job_id" command)" = "$edited_command" ]
  [ -z "$(job_field "$job_id" next_at)" ]
  rm -f "$TEST_ROOT/child.pid"
  RAT_OWNED_CHILD_PID=""
}

@test "deletion prevents future execution and retains existing history" {
  start_daemon
  local completed_command future_command marker completed_id future_id
  marker="$TEST_ROOT/future-delete.marker"
  completed_command='printf completed-before-delete'
  run "$RAT_BIN" add 0 -- "$completed_command"
  [ "$status" -eq 0 ]
  completed_id="$(job_id_for_command "$completed_command")"
  wait_for_history_status "$completed_id" succeeded
  run "$RAT_BIN" delete "$completed_id"
  [ "$status" -eq 0 ]
  [ "$(history_count "$completed_id")" -eq 1 ]

  future_command="printf should-not-run >> '$marker'"
  run "$RAT_BIN" add 4 -- "$future_command"
  [ "$status" -eq 0 ]
  future_id="$(job_id_for_command "$future_command")"
  run "$RAT_BIN" delete "$future_id"
  [ "$status" -eq 0 ]
  sleep 4.3
  [ ! -e "$marker" ]
  [ "$(history_count "$future_id")" -eq 0 ]
}

@test "recurring add saves its IANA timezone and calendar rule" {
  start_daemon
  local command job_id schedule
  command='true'
  run "$RAT_BIN" add --cron '0 9 * * *' --timezone Pacific/Auckland -- "$command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$command")"
  schedule="$(job_field "$job_id" schedule)"
  [[ "$schedule" == *"Pacific/Auckland"* ]]
  [[ "$schedule" == *"0 9 * * *"* ]]
  local next_at
  next_at="$(job_field "$job_id" next_at)"
  python3 -c 'from datetime import datetime; import sys; value=datetime.fromisoformat(sys.argv[1].replace("Z","+00:00")); assert value.tzinfo is not None' "$next_at"
}

@test "failed one-off is terminal and is never automatically retried" {
  start_daemon
  local marker command job_id
  marker="$TEST_ROOT/failure.marker"
  command="printf failed-once >> '$marker'; exit 23"
  run "$RAT_BIN" add 0 -- "$command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$command")"
  wait_for_history_status "$job_id" failed
  sleep 1.2
  [ "$(line_count "$marker")" -eq 1 ]
  [ "$(history_count "$job_id")" -eq 1 ]
  [ "$(printf '%s' "$(job_field "$job_id" state)" | tr '[:upper:]' '[:lower:]')" = "completed" ]
}

@test "restart catches up one overdue one-off exactly once" {
  start_daemon
  local marker command job_id
  marker="$TEST_ROOT/catchup.marker"
  command="printf caught-up >> '$marker'"
  run "$RAT_BIN" add 5 -- "$command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$command")"
  stop_daemon
  sleep 5.3

  start_daemon
  wait_for_file "$marker"
  wait_for_history_status "$job_id" succeeded
  [ "$(line_count "$marker")" -eq 1 ]
}

@test "crash after a concrete side effect moves the run to review and never retries" {
  start_daemon
  local marker pid_file command job_id
  marker="$TEST_ROOT/uncertain.marker"
  pid_file="$TEST_ROOT/child.pid"
  command="printf 'run\\n' >> '$marker'; printf '%s' \"\$\$\" > '$pid_file'; exec sleep 60"
  run "$RAT_BIN" add 0 -- "$command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$command")"

  wait_for_file "$marker"
  wait_for_file "$pid_file"
  RAT_OWNED_CHILD_PID="$(cat "$pid_file")"
  case "$RAT_OWNED_CHILD_PID" in ''|*[!0-9]*) printf 'command wrote an invalid child PID\n' >&2; return 1 ;; esac

  kill -KILL "$RAT_DAEMON_PID"
  wait "$RAT_DAEMON_PID" 2>/dev/null || true
  RAT_DAEMON_PID=""
  start_daemon
  wait_for_job_state "$job_id" needsreview
  sleep 1.2
  [ "$(line_count "$marker")" -eq 1 ]
  run "$RAT_BIN" review "$job_id" --acknowledge
  [ "$status" -eq 0 ]
  sleep 0.5
  [ "$(line_count "$marker")" -eq 1 ]
  cleanup_command_child
}

@test "first interactive setup asks once after list created the database and records decline" {
  python3 - "$RAT_BIN" "$RAT_HOME" "$TEST_ROOT" <<'PY'
import errno
import os
import pty
import select
import subprocess
import sys
import time
from pathlib import Path

binary, root_name, test_root_name = sys.argv[1:]
root = Path(root_name)
test_root = Path(test_root_name)
home = test_root / "isolated-home"
home.mkdir()
config_home = home / "config"
env = os.environ.copy()
env.update(RAT_HOME=str(root), HOME=str(home), XDG_CONFIG_HOME=str(config_home))

listed = subprocess.run(
    [binary, "list", "--json"],
    cwd=test_root,
    env=env,
    capture_output=True,
    text=True,
    timeout=5,
)
assert listed.returncode == 0, listed.stderr
assert any(path.is_file() for path in root.iterdir()), "list did not create the database"
decision = root / ".first-interactive-setup"
assert not decision.exists()

def start_ui():
    master, slave = pty.openpty()
    process = subprocess.Popen(
        [binary], cwd=test_root, env=env, stdin=slave, stdout=slave, stderr=slave
    )
    os.close(slave)
    return process, master

def read_until(process, master, expected):
    deadline = time.monotonic() + 8
    output = bytearray()
    while expected not in output:
        if process.poll() is not None:
            raise AssertionError(f"rat exited before {expected!r}: {output!r}")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(f"timed out waiting for {expected!r}: {output!r}")
        ready, _, _ = select.select([master], [], [], remaining)
        if not ready:
            raise AssertionError(f"timed out waiting for {expected!r}: {output!r}")
        try:
            chunk = os.read(master, 4096)
        except OSError as error:
            if error.errno == errno.EIO:
                raise AssertionError(f"rat closed the PTY before {expected!r}") from error
            raise
        output.extend(chunk)
    return bytes(output)

def stop_ui(process, master):
    if process.poll() is None:
        process.kill()
    process.wait(timeout=5)
    os.close(master)

process, master = start_ui()
try:
    prompt = read_until(
        process,
        master,
        b"First setup: install rat to start automatically at login? [y/N] ",
    )
    assert b"First setup:" in prompt
    os.write(master, b"n\n")
    read_until(process, master, b"rat> ")
    os.write(master, b"q\n")
    assert process.wait(timeout=5) == 0
finally:
    stop_ui(process, master)

assert decision.read_text() == "declined\n"
assert not (home / "Library/LaunchAgents/io.github.smartcrabai.rat.plist").exists()
assert not (config_home / "systemd/user/rat.service").exists()
assert not (home / ".config/systemd/user/rat.service").exists()

process, master = start_ui()
try:
    startup = read_until(process, master, b"rat> ")
    assert b"First setup:" not in startup
    os.write(master, b"q\n")
    assert process.wait(timeout=5) == 0
finally:
    stop_ui(process, master)
PY
}

@test "interactive help and output command show captured run output" {
  start_daemon
  local command job_id run_id
  command='printf interactive-output-token'
  run "$RAT_BIN" add 0 -- "$command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$command")"
  wait_for_history_status "$job_id" succeeded
  run_id="$RAT_LAST_RUN_ID"
  printf 'declined\n' >"$RAT_HOME/.first-interactive-setup"

  python3 - "$RAT_BIN" "$RAT_HOME" "$TEST_ROOT" "$run_id" <<'PY'
import errno
import os
import pty
import select
import subprocess
import sys
import time
from pathlib import Path

binary, root_name, test_root_name, run_id = sys.argv[1:]
root = Path(root_name)
test_root = Path(test_root_name)
home = test_root / "interactive-home"
home.mkdir()
env = os.environ.copy()
env.update(
    RAT_HOME=str(root),
    HOME=str(home),
    XDG_CONFIG_HOME=str(home / "config"),
)
master, slave = pty.openpty()
process = subprocess.Popen(
    [binary], cwd=test_root, env=env, stdin=slave, stdout=slave, stderr=slave
)
os.close(slave)

def read_until(expected):
    deadline = time.monotonic() + 8
    output = bytearray()
    while expected not in output:
        if process.poll() is not None:
            raise AssertionError(f"rat exited before {expected!r}: {output!r}")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(f"timed out waiting for {expected!r}: {output!r}")
        ready, _, _ = select.select([master], [], [], remaining)
        if not ready:
            raise AssertionError(f"timed out waiting for {expected!r}: {output!r}")
        try:
            output.extend(os.read(master, 4096))
        except OSError as error:
            if error.errno == errno.EIO:
                raise AssertionError(f"rat closed the PTY before {expected!r}") from error
            raise
    return bytes(output)

try:
    read_until(b"rat> ")
    os.write(master, b"help\n")
    help_output = read_until(b"rat> ")
    assert b"output RUN_ID" in help_output
    os.write(master, f"output {run_id}\n".encode())
    output = read_until(b"rat> ")
    assert b"interactive-output-token" in output
    assert b"unknown interactive command" not in output
    os.write(master, b"q\n")
    assert process.wait(timeout=5) == 0
finally:
    if process.poll() is None:
        process.kill()
    process.wait(timeout=5)
    os.close(master)
PY
}

@test "add requires a live scheduler even when a LaunchAgent reports loaded" {
  if [[ "$(uname -s)" != "Darwin" ]]; then
    skip "LaunchAgent liveness regression applies to macOS"
  fi

  local isolated_home="$TEST_ROOT/fake-home"
  local fake_bin="$TEST_ROOT/fake-bin"
  local launchctl_log="$TEST_ROOT/launchctl.log"
  mkdir -p "$isolated_home" "$fake_bin"
  python3 - "$isolated_home" "$RAT_HOME" "$RAT_BIN" <<'PY'
from pathlib import Path
import sys

home, root, binary = map(Path, sys.argv[1:])
agent = home / "Library/LaunchAgents/io.github.smartcrabai.rat.plist"
agent.parent.mkdir(parents=True)
agent.write_text(
    '<?xml version="1.0"?>\n'
    '<plist><dict>\n'
    '<key>Label</key>\n<string>io.github.smartcrabai.rat</string>\n'
    '<key>ProgramArguments</key>\n'
    f'<array><string>{binary}</string><string>daemon</string></array>\n'
    '<key>EnvironmentVariables</key>\n<dict>\n'
    '\t\t<key>RAT_HOME</key>\n'
    f'\t\t<string>{root}</string>\n'
    '</dict>\n</dict></plist>\n'
)
PY
  cat >"$fake_bin/launchctl" <<'SH'
#!/bin/sh
printf '%s\n' "$*" >>"$LAUNCHCTL_LOG"
exit 0
SH
  chmod +x "$fake_bin/launchctl"

  run env HOME="$isolated_home" RAT_HOME="$RAT_HOME" \
    PATH="$fake_bin:$PATH" LAUNCHCTL_LOG="$launchctl_log" \
    "$RAT_BIN" add 3600 -- true 2>&1
  [ "$status" -ne 0 ]
  [[ "$output" == *"scheduler is not running"* ]]
  [ ! -e "$launchctl_log" ]
  [ "$(job_count)" -eq 0 ]
}
