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

  RAT_SERVICE_DAEMON_PID_FILE="$TEST_ROOT/service-daemon.pid"
  RAT_DAEMON_PID=""
  RAT_OWNED_CHILD_PID=""
  RAT_LAST_RUN_ID=""
}

teardown() {
  stop_daemon
  stop_service_daemon
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

stop_service_daemon() {
  local pid attempt
  [[ -s "$RAT_SERVICE_DAEMON_PID_FILE" ]] || return 0
  pid="$(cat "$RAT_SERVICE_DAEMON_PID_FILE")"
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null || true
    for attempt in {1..50}; do
      if ! kill -0 "$pid" 2>/dev/null; then break; fi
      sleep 0.1
    done
    kill -KILL "$pid" 2>/dev/null || true
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

ui_session() {
  python3 - "$RAT_BIN" "$RAT_HOME" "$TEST_ROOT" "$@" <<'PY'
import errno
import fcntl
import os
import pty
import select
import re
import signal
import struct
import sys
import termios
import time
from pathlib import Path

binary, rat_home_name, test_root_name, *steps = sys.argv[1:]
test_root = Path(test_root_name)
ui_home = test_root / "ui-home"
ui_home.mkdir(exist_ok=True)
native_bin = test_root / "ui-native-bin"
native_bin.mkdir(exist_ok=True)
native_tools = {
    "at": '#!/bin/sh\nif [ "$1" = "-l" ]; then exit 0; fi\nexit 1\n',
    "atq": '#!/bin/sh\nexit 0\n',
    "crontab": '#!/bin/sh\nif [ "$1" = "-l" ]; then printf "no crontab for %s\\n" "$(id -un)" >&2; exit 1; fi\nexit 1\n',
    "launchctl": '#!/bin/sh\nexit 1\n',
    "systemctl": '#!/bin/sh\nexit 1\n',
}
for name, script in native_tools.items():
    tool = native_bin / name
    tool.write_text(script)
    tool.chmod(0o700)
env = os.environ.copy()
env.update(
    RAT_BIN=binary,
    RAT_HOME=rat_home_name,
    HOME=str(ui_home),
    XDG_CONFIG_HOME=str(ui_home / "config"),
    TERM="xterm-256color",
    PATH=f"{native_bin}{os.pathsep}{os.environ['PATH']}",
)
for step in steps:
    if step.startswith("env:"):
        name, _, value = step[4:].partition("=")
        env[name] = value
service_bin = env.pop("RAT_TEST_SERVICE_BIN", None)
if service_bin:
    env["PATH"] = f"{service_bin}{os.pathsep}{env['PATH']}"

pid, master = pty.fork()
if pid == 0:
    try:
        os.chdir(test_root)
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        os.closerange(3, os.sysconf("SC_OPEN_MAX"))
        os.execve(binary, [binary], env)
    except OSError as error:
        os.write(2, f"could not exec rat: {error}\n".encode())
        os._exit(127)
fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
os.set_blocking(master, False)
transcript = bytearray()
cursor = 0
finished = None

def poll():
    global finished
    if finished is not None:
        return finished
    child, status = os.waitpid(pid, os.WNOHANG)
    if child:
        finished = os.waitstatus_to_exitcode(status)
    return finished

def drain_available():
    while select.select([master], [], [], 0)[0]:
        try:
            chunk = os.read(master, 4096)
        except BlockingIOError:
            return
        except OSError as error:
            if error.errno == errno.EIO:
                return
            raise
        if not chunk:
            return
        transcript.extend(chunk)

def wait_for_exit(expected):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        drain_available()
        status = poll()
        if status is not None:
            drain_available()
            if status != expected:
                raise AssertionError(f"rat exited with status {status}, expected {expected}")
            return status
        ready, _, _ = select.select([master], [], [], 0.05)
        if ready:
            drain_available()
    raise AssertionError(f"rat did not exit with status {expected}")

def assert_terminal_restored():
    if not (termios.tcgetattr(master)[3] & termios.ECHO):
        raise AssertionError("terminal ECHO was not restored")
    visible = True
    for state in re.findall(rb"\x1b\[\?25([hl])", transcript):
        visible = state == b"h"
    if not visible:
        raise AssertionError("terminal cursor was not visible at exit")

def read_until(expected):
    global cursor
    expected = expected.encode()
    deadline = time.monotonic() + 8
    while expected not in transcript[cursor:]:
        if poll() is not None:
            raise AssertionError(f"rat exited before {expected!r}")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError(f"timed out waiting for {expected!r}")
        ready, _, _ = select.select([master], [], [], remaining)
        if not ready:
            raise AssertionError(f"timed out waiting for {expected!r}")
        try:
            chunk = os.read(master, 4096)
        except BlockingIOError:
            continue
        except OSError as error:
            if error.errno == errno.EIO:
                raise AssertionError(f"rat closed the PTY before {expected!r}") from error
            raise
        if not chunk:
            raise AssertionError(f"rat closed the PTY before {expected!r}")
        transcript.extend(chunk)
    cursor = transcript.find(expected, cursor) + len(expected)

def send_key(key, count=1):
    keys = {"ENTER": b"\r", "DOWN": b"\x1b[B", "ESC": b"\x1b", "CTRL_C": b"\x03"}
    for _ in range(count):
        os.write(master, keys[key])

expected_exit = None
try:
    for step in steps:
        kind, _, value = step.partition(":")
        if kind == "env":
            continue
        if kind == "expect":
            read_until(value)
        elif kind == "key":
            key, separator, count = value.partition("*")
            send_key(key, int(count) if separator else 1)
        elif kind == "text":
            os.write(master, value.encode())
        elif kind == "signal" and value == "INT":
            os.kill(pid, signal.SIGINT)
        elif kind == "normal-exit":
            expected_exit = wait_for_exit(0)
            assert_terminal_restored()
    if expected_exit is None and poll() is not None:
        raise AssertionError("rat exited during the interactive flow")
except Exception:
    print(transcript.decode(errors="replace"), file=sys.stderr)
    raise
finally:
    # macOS can block child exit until its PTY master is drained or closed.
    os.close(master)
    if poll() is None:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 5
    while poll() is None and time.monotonic() < deadline:
        time.sleep(0.05)
    if poll() is None:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 5
        while poll() is None and time.monotonic() < deadline:
            time.sleep(0.05)
        if poll() is None:
            raise AssertionError("rat did not exit after SIGKILL")

print(transcript.decode(errors="replace"), end="")
PY
}

job_row_downs() {
  "$RAT_BIN" list --all --json | python3 -c 'import json,sys; d=json.load(sys.stdin); rows=d if isinstance(d,list) else next((d[k] for k in ("jobs","items","data") if isinstance(d,dict) and isinstance(d.get(k),list)), None); assert isinstance(rows,list), "expected a job list"; wanted=sys.argv[1]; matches=[i for i,j in enumerate(rows) if str(j.get("id"))==wanted]; assert len(matches)==1, f"expected one row for job {wanted}, found {len(matches)}"; print(3+matches[0])' "$1"
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

@test "interactive startup does not install a service" {
  local ui_home="$TEST_ROOT/ui-home"
  run ui_session 'expect:Add a job' 'expect:Quit'
  [ "$status" -eq 0 ]
  [ ! -e "$ui_home/Library/LaunchAgents/io.github.smartcrabai.rat.plist" ]
  [ ! -e "$ui_home/config/systemd/user/rat.service" ]
  [ ! -e "$ui_home/.config/systemd/user/rat.service" ]
}

@test "Ctrl-C at the main menu exits normally and restores the terminal" {
  run ui_session 'expect:Add a job' 'signal:INT' 'normal-exit'
  [ "$status" -eq 0 ]
}

@test "Ctrl-C in an interactive text field cancels to the list and restores echo" {
  start_daemon
  local command ready_id before_jobs before_history
  command='printf should-not-be-added'
  ready_id="$(job_id_for_command ': rat-e2e-readiness')"
  before_jobs="$("$RAT_BIN" list --all --json)"
  before_history="$("$RAT_BIN" history "$ready_id" --json)"

  run ui_session \
    'expect:Add a job' \
    'key:ENTER' \
    'expect:One-off (at)' \
    'key:DOWN*2' \
    'key:ENTER' \
    'expect:First setup: install rat to start automatically at login?' \
    'key:ENTER' \
    'expect:Schedule [oneoff/daily/weekly/monthly]:' \
    'text:oneoff' \
    'key:ENTER' \
    'expect:One-off time tokens (for example 2h 5m or 20270101T090000Z):' \
    'text:2h' \
    'key:ENTER' \
    'expect:IANA timezone [' \
    'key:ENTER' \
    'expect:Command string (one shell command):' \
    "text:$command" \
    'key:CTRL_C' \
    'expect:Add a job' \
    'key:ESC' \
    'normal-exit'
  [ "$status" -eq 0 ]
  [ "$("$RAT_BIN" list --all --json)" = "$before_jobs" ]
  [ "$("$RAT_BIN" history "$ready_id" --json)" = "$before_history" ]
}

@test "cancelling an interactive rat add leaves jobs and history unchanged" {
  start_daemon
  local command ready_id before_jobs before_history
  command='true'
  ready_id="$(job_id_for_command ': rat-e2e-readiness')"
  before_jobs="$("$RAT_BIN" list --all --json)"
  before_history="$("$RAT_BIN" history "$ready_id" --json)"

  run ui_session \
    'expect:Add a job' \
    'key:ENTER' \
    'expect:One-off (at)' \
    'key:DOWN*2' \
    'key:ENTER' \
    'expect:First setup: install rat to start automatically at login?' \
    'key:ENTER' \
    'expect:Schedule [oneoff/daily/weekly/monthly]:' \
    'text:oneoff' \
    'key:ENTER' \
    'expect:One-off time tokens (for example 2h 5m or 20270101T090000Z):' \
    'text:2h' \
    'key:ENTER' \
    'expect:IANA timezone [' \
    'key:ENTER' \
    'expect:Command string (one shell command):' \
    "text:$command" \
    'key:ENTER' \
    'expect:env>' \
    'key:ENTER' \
    'expect:Add command' \
    'text:n' \
    'key:ENTER' \
    'expect:Add a job'
  [ "$status" -eq 0 ]
  [ "$("$RAT_BIN" list --all --json)" = "$before_jobs" ]
  [ "$("$RAT_BIN" history "$ready_id" --json)" = "$before_history" ]
}

@test "interactive rat addition persists and runs through the live daemon" {
  start_daemon
  local command marker job_id
  marker="$TEST_ROOT/interactive-added.marker"
  command="printf interactive-added-token > '$marker'"

  run ui_session \
    'expect:Add a job' \
    'key:ENTER' \
    'expect:One-off (at)' \
    'key:DOWN*2' \
    'key:ENTER' \
    'expect:First setup: install rat to start automatically at login?' \
    'key:ENTER' \
    'expect:Schedule [oneoff/daily/weekly/monthly]:' \
    'text:oneoff' \
    'key:ENTER' \
    'expect:One-off time tokens (for example 2h 5m or 20270101T090000Z):' \
    'text:0' \
    'key:ENTER' \
    'expect:IANA timezone [' \
    'key:ENTER' \
    'expect:Command string (one shell command):' \
    "text:$command" \
    'key:ENTER' \
    'expect:env>' \
    'key:ENTER' \
    'expect:Add command' \
    'text:y' \
    'key:ENTER' \
    'expect:Add a job'
  [ "$status" -eq 0 ]

  job_id="$(job_id_for_command "$command")"
  wait_for_file "$marker"
  wait_for_history_status "$job_id" succeeded
  [ "$(cat "$marker")" = "interactive-added-token" ]
}

@test "interactive first install waits for a real daemon before scheduling" {
  case "$(uname -s)" in
    Linux|Darwin) ;;
    *) skip "isolated service-manager readiness regression applies to Linux and macOS" ;;
  esac
  local service_bin="$TEST_ROOT/fake-service-bin"
  local marker="$TEST_ROOT/first-install.marker"
  local command job_id unit_path
  command="printf first-install-token > '$marker'"
  mkdir -p "$service_bin"

  cat >"$service_bin/start-rat-daemon" <<'SH'
#!/bin/sh
exec python3 - "$RAT_BIN" "$RAT_TEST_DAEMON_PID_FILE" "$RAT_TEST_DAEMON_LOG" "$RAT_TEST_DAEMON_DELAY" <<'PY'
import os
import subprocess
import sys

binary, pid_file, log_file, delay = sys.argv[1:]
launcher = "import os,sys,time; time.sleep(float(sys.argv[1])); os.execv(sys.argv[2], [sys.argv[2], 'daemon'])"
with open(log_file, "ab", buffering=0) as output:
    daemon = subprocess.Popen(
        [sys.executable, "-c", launcher, delay, binary],
        env=os.environ.copy(),
        stdin=subprocess.DEVNULL,
        stdout=output,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
with open(pid_file, "w", encoding="ascii") as tracked:
    tracked.write(f"{daemon.pid}\n")
PY
SH
  chmod +x "$service_bin/start-rat-daemon"

  if [[ "$(uname -s)" == "Linux" ]]; then
    cat >"$service_bin/systemctl" <<'SH'
#!/bin/sh
case "$*" in
  "--user show-environment") exit 0 ;;
  "--user show rat.service --property=FragmentPath --value") exit 0 ;;
  "--user is-active --quiet rat.service") exit 1 ;;
  "--user daemon-reload") exit 0 ;;
  "--user enable --now rat.service") "$(dirname "$0")/start-rat-daemon" ;;
  *) exit 1 ;;
esac
SH
    unit_path="$TEST_ROOT/ui-home/config/systemd/user/rat.service"
  else
    cat >"$service_bin/launchctl" <<'SH'
#!/bin/sh
case "$1" in
  print) exit 1 ;;
  bootstrap) "$(dirname "$0")/start-rat-daemon" ;;
  *) exit 1 ;;
esac
SH
    unit_path="$TEST_ROOT/ui-home/Library/LaunchAgents/io.github.smartcrabai.rat.plist"
  fi
  if [[ "$(uname -s)" == "Linux" ]]; then
    chmod +x "$service_bin/systemctl"
  else
    chmod +x "$service_bin/launchctl"
  fi

  run ui_session \
    "env:RAT_TEST_SERVICE_BIN=$service_bin" \
    "env:RAT_TEST_DAEMON_PID_FILE=$RAT_SERVICE_DAEMON_PID_FILE" \
    "env:RAT_TEST_DAEMON_LOG=$TEST_ROOT/service-daemon.log" \
    'env:RAT_TEST_DAEMON_DELAY=1.0' \
    'expect:Add a job' \
    'key:ENTER' \
    'expect:One-off (at)' \
    'key:DOWN*2' \
    'key:ENTER' \
    'expect:First setup: install rat to start automatically at login?' \
    'text:y' \
    'key:ENTER' \
    "expect:Installed rat's login service." \
    'expect:Schedule [oneoff/daily/weekly/monthly]:' \
    'text:oneoff' \
    'key:ENTER' \
    'expect:One-off time tokens (for example 2h 5m or 20270101T090000Z):' \
    'text:0' \
    'key:ENTER' \
    'expect:IANA timezone [' \
    'key:ENTER' \
    'expect:Command string (one shell command):' \
    "text:$command" \
    'key:ENTER' \
    'expect:env>' \
    'key:ENTER' \
    'expect:Add command' \
    'text:y' \
    'key:ENTER' \
    'expect:Add a job' \
    'key:ESC' \
    'normal-exit'
  [ "$status" -eq 0 ]
  [ -e "$unit_path" ]
  [ -s "$RAT_SERVICE_DAEMON_PID_FILE" ]

  job_id="$(job_id_for_command "$command")"
  [ "$(job_field "$job_id" command)" = "$command" ]
  wait_for_file "$marker"
  wait_for_history_status "$job_id" succeeded
  [ "$(cat "$marker")" = "first-install-token" ]
}

@test "selecting and pausing a rat row updates persisted state" {
  start_daemon
  local command job_id row_downs initial_state
  command='true'
  run "$RAT_BIN" add 86400 -- "$command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$command")"
  row_downs="$(job_row_downs "$job_id")"
  initial_state="$(job_field "$job_id" state | tr '[:upper:]' '[:lower:]' | tr -d '_-')"
  [ "$initial_state" != "paused" ]

  run ui_session \
    'expect:Add a job' \
    "expect:rat job #$job_id" \
    "key:DOWN*$row_downs" \
    'key:ENTER' \
    'expect:Pause job' \
    'key:DOWN*2' \
    'key:ENTER' \
    'expect:Add a job'
  [ "$status" -eq 0 ]
  [ "$(job_field "$job_id" state | tr '[:upper:]' '[:lower:]' | tr -d '_-')" = "paused" ]
}

@test "run history selection shows captured output from a real run" {
  start_daemon
  local command job_id run_id
  command='printf "selected-%s" output-token'
  run "$RAT_BIN" add 0 -- "$command"
  [ "$status" -eq 0 ]
  job_id="$(job_id_for_command "$command")"
  wait_for_history_status "$job_id" succeeded
  run_id="$RAT_LAST_RUN_ID"

  run ui_session \
    'expect:Add a job' \
    'expect:Run history (rat)' \
    'key:DOWN*2' \
    'key:ENTER' \
    'expect:Select run output' \
    "expect:run #$run_id" \
    'key:ENTER' \
    'expect:stdout:' \
    'expect:selected-output-token'
  [ "$status" -eq 0 ]
  [[ "$output" == *"selected-output-token"* ]]
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
