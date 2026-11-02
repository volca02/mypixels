#!/usr/bin/env bash
# Drive the real spike UI through the $SPIKE_KEYS test hook and profile the
# live render loop with perf. Redraws happen when the app is woken, and
# every FIFO line wakes it, so spamming "spy" (a hook line that only logs)
# is a steady frame pump.
#
#   scripts/ui_bench.sh                 # all scenarios, in order
#   scripts/ui_bench.sh config          # one scenario only
#
# Scenarios (cumulative state, like a real session):
#   terminal     terminal streaming output (yes), no UI open
#   config       config panel open WHILE the terminal streams (the pain combo)
#   config-idle  config panel open, terminal quiet (pure UI frame cost)
#
# Output: one BENCH line + a top-symbols table per scenario, plus
# /tmp/spike-perf/perf.<scenario>.data for deeper digging.
# Needs the RELEASE binary: a debug build distorts everything perf says.
set -uo pipefail

SPIKE_DIR=${SPIKE_DIR:-$HOME/Projects/mypixels/spike}
BIN=${BIN:-$SPIKE_DIR/target/release/spike}
SECS=${SECS:-8}
PERF_F=${PERF_F:-299}
OUT=${OUT:-/tmp/spike-perf}
mkdir -p "$OUT"

[ -x "$BIN" ] || { echo "no release binary at $BIN (cargo build --release)"; exit 1; }
command -v perf >/dev/null || { echo "perf not installed"; exit 1; }
pgrep -x spike >/dev/null && { echo "a spike is already running -- quit it first (skews perf and holds the terminal)"; exit 1; }

FIFO=$(mktemp -u /tmp/spike_keys.XXXXXX)
mkfifo "$FIFO"
LOG=$OUT/spike.log
: > "$LOG"

RUST_LOG=${RUST_LOG:-info} SPIKE_TRACE=1 SPIKE_KEYS=$FIFO "$BIN" >"$LOG" 2>&1 &
SPID=$!
exec 3>"$FIFO"
say() { printf '%s\n' "$1" >&3; }
cleanup() { exec 3>&- 2>/dev/null; kill "$SPID" "$PUMP" 2>/dev/null; wait 2>/dev/null; rm -f "$FIFO"; }
PUMP=""
trap cleanup EXIT

# Boot: wait until the hook answers.
for _ in $(seq 100); do
    say spy
    sleep 0.1
    grep -q "SPY active=" "$LOG" && break
done
grep -q "SPY active=" "$LOG" || { echo "spike did not come up; log tail:"; tail -20 "$LOG"; exit 1; }
echo "== spike up (pid $SPID) =="

# Type into the active terminal, one key per FIFO line ("space" is a spec,
# a literal space line would be trimmed away by the reader).
sendkeys() {
    local s=$1 c
    for ((i = 0; i < ${#s}; i++)); do
        c=${s:i:1}
        [ "$c" = " " ] && c=space
        say "$c"
    done
    say "${2:-return}"
}

pump() { # steady frame pump: "spy" lines as fast as the loop allows
    ( while :; do say spy; sleep 0.004; done ) &
    PUMP=$!
}
stpump() { kill "$PUMP" 2>/dev/null; wait "$PUMP" 2>/dev/null; PUMP=""; }

scenario() { # name setup-shell-code [teardown-shell-code]
    local name=$1 setup=$2 teardown=${3:-:}
    local before after tbefore tafter
    before=$(grep -c "SPY active=" "$LOG")
    tbefore=$(grep -c "frame build" "$LOG")
    eval "$setup"
    pump
    sleep 0.5
    # -p attaches to the live app; the time limit is an external sleep + INT.
    perf record -q -o "$OUT/perf.$name.data" -F "$PERF_F" -g --call-graph dwarf -p "$SPID" &
    local PREC=$!
    sleep "$SECS"
    kill -INT "$PREC"
    wait "$PREC" 2>/dev/null
    stpump
    eval "$teardown"
    sleep 0.3
    after=$(grep -c "SPY active=" "$LOG")
    tafter=$(grep -c "frame build" "$LOG")
    echo "BENCH $name  pump_lines=$((after - before))/${SECS}s"
    # build_frame wall time, straight from the app's own SPIKE_TRACE lines,
    # normalized to microseconds: p50/p95/max over this scenario's frames.
    grep -o "frame build [0-9.]*[a-zµ]*s" "$LOG" | tail -n $((tafter - tbefore)) \
        | sed 's/frame build //' \
        | awk '{v=$1+0; if ($0 ~ /ms/) v*=1000; else if ($0 ~ /ns/) v/=1000; print v}' \
        | sort -n \
        | awk '{a[NR]=$1} END {if (NR) printf "      build_frame us: n=%d p50=%.1f p95=%.1f max=%.1f\n", NR, a[int(NR*0.5)], a[int(NR*0.95)], a[NR]}' 
    echo "-- $name: top symbols --"
    perf report -i "$OUT/perf.$name.data" --stdio --percent-limit 0.5 2>/dev/null \
        | grep -v '^#' | grep -v '^[[:space:]]*$' | head -22
    echo
}

# (not a case block: `all` must run EVERY scenario, and case stops at the
# first matching pattern)
RUN=${1:-all}
want() { [ "$RUN" = all ] || [ "$RUN" = "$1" ]; }
want terminal     && scenario terminal     "sendkeys 'yes'"
want config       && scenario config       "say alt+shift+c"
want config-idle  && scenario config-idle  "say ctrl+c"
want switcher     && scenario switcher     "say switcher" "say alt_up"

stpump
say ctrl+c
echo "data: $OUT   flamegraph: perf script -i $OUT/perf.config.data | inferno-collapse-perf | inferno-flamegraph > $OUT/config.svg"
