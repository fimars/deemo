#!/usr/bin/env bash
# End-to-end self-check for deemo, driven through a REAL terminal (tmux).
#
# The benchmark workload is `python3 -m http.server`: a persistent process
# whose request log goes to *stderr*, so it also proves deemo captures both
# fds without `2>&1`.
#
# Usage: scripts/e2e-tmux.sh [path-to-binary]
#   (default: target/release/deemo)

set -uo pipefail

BIN=${1:-${BIN:-target/release/deemo}}
PORT=${PORT:-8931}
DOCROOT=$(mktemp -d)
HOMEDIR=$(mktemp -d)
export DEEMO_HOME=$HOMEDIR
echo "hello e2e" > "$DOCROOT/index.html"

fail() { echo "FAIL: $*" >&2; echo "--- pane tail:"; tmux -S "$SOCKET" capture-pane -p -J -t "$SESSION":0.0 -S -30 2>/dev/null | tail -20; cleanup; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "e2e needs '$1'"; }
need tmux; need python3; need curl
[ -x "$BIN" ] || fail "binary not found: $BIN (run cargo build --release)"

SOCKET_DIR=${CLAUDE_TMUX_SOCKET_DIR:-${TMPDIR:-/tmp}/deemo-tmux-sockets}
mkdir -p "$SOCKET_DIR"
SOCKET="$SOCKET_DIR/e2e.sock"
SESSION="deemo-e2e-$$"

cleanup() {
  tmux -S "$SOCKET" kill-session -t "$SESSION" 2>/dev/null
  [ -n "${LABEL:-}" ] && DEEMO_HOME="$HOMEDIR" "$BIN" stop "$LABEL" >/dev/null 2>&1
  rm -rf "$HOMEDIR" "$DOCROOT"
  return 0
}
trap cleanup EXIT

# Send a line, then wait for a (unique) regex to appear in the pane.
run() {  # literal text, then Enter as a key name (-l swallows key names)
  tmux -S "$SOCKET" send-keys -t "$SESSION":0.0 -l -- "$1"
  tmux -S "$SOCKET" send-keys -t "$SESSION":0.0 Enter
}
wait_for() { # $1 regex, $2 timeout (s)
  local pattern=$1 deadline=$(( SECONDS + ${2:-15} ))
  while (( SECONDS < deadline )); do
    tmux -S "$SOCKET" capture-pane -p -J -t "$SESSION":0.0 -S -2000 | grep -qE "$pattern" && return 0
    sleep 0.4
  done
  echo "FAIL: timed out waiting for: $pattern" >&2
  tmux -S "$SOCKET" capture-pane -p -J -t "$SESSION":0.0 -S -30 | tail -20 >&2
  exit 1
}

# Deterministic shell in the pane. NOTE: the window command is interpreted by
# the user's default shell (may be fish!), so keep it trivially portable and
# set the prompt from inside the pane afterwards.
tmux -S "$SOCKET" -f /dev/null new-session -d -s "$SESSION" -x 220 -y 50 \
  "bash --noprofile --norc"
# wait for the pane's bash to be ready (echo sentinel), then set the prompt
run 'echo BASH-READY-9X'
wait_for 'BASH-READY-9X' 15
run 'export PS1="E2E> "'

LABEL=httpd
echo "== T1: bare 'deemo' in a tty must NOT block (usage error, prompt returns) =="
run "$BIN"
wait_for 'nothing to do' 10

echo "== T2: deemo -- http.server returns to the shell immediately =="
run "$BIN --label $LABEL -- python3 -m http.server $PORT --directory $DOCROOT"
wait_for "detached .*(pid [0-9]+)" 10
run "echo SHELL-BACK"
wait_for 'SHELL-BACK' 5   # shell accepted the next command => deemo returned

echo "== T3: the daemon actually serves HTTP =="
CODE=""
for _ in $(seq 1 20); do
  CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://127.0.0.1:$PORT/" 2>/dev/null) && [ "$CODE" = 200 ] && break
  sleep 0.4
done
[ "$CODE" = 200 ] || fail "http.server did not serve 200 (got: ${CODE:-none})"
echo "   HTTP $CODE"

echo "== T4: deemo ps sees it running =="
run "$BIN ps"
wait_for "running" 10

echo "== T5: request log (stderr!) captured without 2>&1 =="
curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$PORT/" 2>/dev/null
run "$BIN logs $LABEL"
wait_for "GET / " 10

echo "== T6: deemo stop terminates the daemon =="
run "$BIN stop $LABEL"
wait_for "stopped $LABEL" 10
sleep 0.5
CODE2=$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://127.0.0.1:$PORT/" 2>/dev/null || true)
[ "${CODE2:-000}" != 200 ] || fail "server still serving after stop"
run "$BIN ps"
wait_for 'no processes' 10

echo "== T7: --foreground stays attached and propagates the exit code =="
run "$BIN --foreground --label fgtest -- sh -c 'echo fg-out; exit 3'"
wait_for 'fg-out' 10
wait_for 'exit status: 3' 10

echo "== T8: detach survives the terminal being closed =="
run "$BIN --label survivor -- sleep 30"
wait_for "detached .*(pid [0-9]+)" 10
tmux -S "$SOCKET" kill-session -t "$SESSION"
sleep 0.5
DEEMO_HOME="$HOMEDIR" "$BIN" ps | grep -q "survivor" \
  || fail "detached process must survive terminal close"
DEEMO_HOME="$HOMEDIR" "$BIN" stop survivor >/dev/null
LABEL=survivor   # for cleanup

echo
echo "ALL E2E CHECKS PASSED (T1-T8)"
