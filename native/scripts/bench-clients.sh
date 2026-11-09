#!/usr/bin/env bash
# Head-to-head startup/memory/CPU benchmark of two Synara desktop clients
# under Xvfb. Runs on the sandbox, not the laptop.
#
#   bench-clients.sh LABEL RUNS SEED_HOME -- COMMAND...
#
# COMMAND is started RUNS times, each with a fresh copy of SEED_HOME in
# $BENCH_HOME (the command must use it: SYNARA_HOME=$BENCH_HOME or
# --home $BENCH_HOME). Per run it records:
#   map_ms     launch -> first visible window named Synara
#   ready_ms   launch -> last change bigger than NOISE_PX pixels before a
#              QUIET_MS quiet spell, watching READY_CROP (WxH+X+Y, such as the
#              sidebar thread list) or the whole screen
#   client_*   PSS/CPU/threads of the client processes (the launched tree minus
#              any server: a process with SYNARA_DESKTOP_PARENT_STDIN=1 and its
#              descendants)
#   total_pss  client plus server
# CPU is measured over IDLE_S seconds after ready. One CSV line per run on
# stdout.
set -uo pipefail

label="$1" runs="$2" seed="$3"
shift 4
display="${BENCH_DISPLAY:-:97}"
idle_s="${IDLE_S:-10}"
timeout_s="${READY_TIMEOUT_S:-90}"
export DISPLAY="$display"

if ! xdpyinfo -display "$display" >/dev/null 2>&1; then
  Xvfb "$display" -screen 0 1440x900x24 -nolisten tcp >/dev/null 2>&1 &
  sleep 1
fi

now_ms() { date +%s%3N; }

descendants() {
  local pid="$1" kid
  echo "$pid"
  for kid in $(cat /proc/"$pid"/task/*/children 2>/dev/null); do descendants "$kid"; done
}

is_server_root() {
  tr '\0' '\n' < /proc/"$1"/environ 2>/dev/null | grep -qx 'SYNARA_DESKTOP_PARENT_STDIN=1'
}

# Prints "client_pids|server_pids".
split_tree() {
  local root="$1" pid server="" client=""
  for pid in $(descendants "$root"); do
    [[ -d /proc/$pid ]] || continue
    if [[ " $server " == *" $pid "* ]]; then continue; fi
    if is_server_root "$pid"; then
      server+=" $(descendants "$pid" | tr '\n' ' ')"
    fi
  done
  for pid in $(descendants "$root"); do
    [[ " $server " == *" $pid "* ]] || client+=" $pid"
  done
  echo "$client|$server"
}

pss_kib() {
  local sum=0 pid v
  for pid in $1; do
    v=$(awk '/^Pss:/ {print $2}' /proc/"$pid"/smaps_rollup 2>/dev/null)
    sum=$((sum + ${v:-0}))
  done
  echo "$sum"
}

cpu_ticks() {
  local sum=0 pid v
  for pid in $1; do
    v=$(awk '{print $14 + $15}' /proc/"$pid"/stat 2>/dev/null)
    sum=$((sum + ${v:-0}))
  done
  echo "$sum"
}

threads() {
  local sum=0 pid
  for pid in $1; do
    [[ -d /proc/$pid/task ]] && sum=$((sum + $(ls /proc/"$pid"/task | wc -l)))
  done
  echo "$sum"
}


hz=$(getconf CLK_TCK)
echo "label,run,map_ms,ready_ms,client_procs,client_threads,client_pss_mib,total_pss_mib,client_idle_cpu_pct"
for run in $(seq 1 "$runs"); do
  if xdotool search --name Synara >/dev/null 2>&1; then
    echo "a Synara window is already open on $display; close it first" >&2
    exit 1
  fi
  export BENCH_HOME
  BENCH_HOME="$(mktemp -d /tmp/bench-home.XXXX)"
  cp -a "$seed"/. "$BENCH_HOME"/
  sync
  echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null 2>&1 || true

  t0=$(now_ms)
  setsid "$@" >"/tmp/bench-$label-$run.log" 2>&1 &
  root=$!

  map_ms=NA
  deadline=$((t0 + timeout_s * 1000))
  while (( $(now_ms) < deadline )); do
    tree=" $(descendants "$root" | tr '\n' ' ') "
    for w in $(xdotool search --onlyvisible --name Synara 2>/dev/null); do
      [[ "$tree" == *" $(xdotool getwindowpid "$w" 2>/dev/null) "* ]] && map_ms=$(( $(now_ms) - t0 ))
    done
    [[ "$map_ms" != NA ]] && break
    sleep 0.02
  done

  # Settled: no change bigger than NOISE_PX pixels (a caret blink, a spinner)
  # for QUIET_MS. ready_ms is when the last big change was seen.
  ready_ms=NA prev="/tmp/bench-prev.png" cur="/tmp/bench-cur.png"
  grab() {
    if [[ -n "${READY_CROP:-}" ]]; then
      import -display "$display" -window root -crop "$READY_CROP" +repage "$1" 2>/dev/null
    else
      import -display "$display" -window root "$1" 2>/dev/null
    fi
  }
  grab "$prev"
  last_change=$(now_ms)
  while (( $(now_ms) < deadline )); do
    grab "$cur"; t=$(now_ms)
    px=$(compare -metric AE "$prev" "$cur" null: 2>&1 | awk '{print int($1)}')
    if (( ${px:-0} > ${NOISE_PX:-500} )); then last_change=$t; mv "$cur" "$prev"
    elif (( t - last_change >= ${QUIET_MS:-4000} )); then ready_ms=$(( last_change - t0 )); break
    fi
  done
  import -display "$display" -window root "/tmp/bench-$label-$run.png" 2>/dev/null

  sleep 2
  IFS='|' read -r client server <<<"$(split_tree "$root")"
  c0=$(cpu_ticks "$client")
  sleep "$idle_s"
  IFS='|' read -r client server <<<"$(split_tree "$root")"
  c1=$(cpu_ticks "$client")
  cpu_pct=$(awk -v d=$((c1 - c0)) -v hz="$hz" -v s="$idle_s" 'BEGIN {printf "%.2f", 100 * d / hz / s}')
  cpss=$(pss_kib "$client"); spss=$(pss_kib "$server")
  nproc_client=$(wc -w <<<"$client")
  echo "$label,$run,$map_ms,$ready_ms,$nproc_client,$(threads "$client"),$((cpss / 1024)),$(((cpss + spss) / 1024)),$cpu_pct"

  kill -TERM -- -"$root" 2>/dev/null
  for _ in $(seq 1 100); do kill -0 "$root" 2>/dev/null || break; sleep 0.1; done
  kill -KILL -- -"$root" 2>/dev/null
  pkill -KILL -f "$BENCH_HOME" 2>/dev/null
  for _ in $(seq 1 50); do xdotool search --name Synara >/dev/null 2>&1 || break; sleep 0.1; done
  sleep 1
  rm -rf "$BENCH_HOME"
done
