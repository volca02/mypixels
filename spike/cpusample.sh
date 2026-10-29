#!/bin/bash
# Sample cumulative CPU% of a process tree for a wall duration.
# Usage: cpusample.sh <pid> <seconds>
pid=$1
secs=$2
HZ=$(getconf CLK_TCK)
read -r _ u1 s1 _ _ _ _ _ _ _ _ _ _ _ _ < /proc/$pid/stat
start=$(date +%s.%N)
peak=0
last_u=$u1; last_s=$s1; last_t=$start
while true; do
  now=$(date +%s.%N)
  el=$(echo "$now $start" | awk '{print $1-$2}')
  [ "$(echo "$el > $secs" | bc)" = "1" ] && break
  if [ -r /proc/$pid/stat ]; then
    read -r _ u2 s2 _ _ _ _ _ _ _ _ _ _ _ _ < /proc/$pid/stat
    dt=$(echo "$now $last_t" | awk '{print $1-$2}')
    dcpu=$(echo "($u2 + $s2 - $last_u - $last_s) / $HZ" | awk '{print $1}')
    inst=$(echo "$dcpu / $dt * 100" | awk '{printf "%.0f", $1}')
    [ "$inst" -gt "$peak" ] && peak=$inst
    last_u=$u2; last_s=$s2; last_t=$now
  fi
  sleep 0.2
done
read -r _ u3 s3 _ _ _ _ _ _ _ _ _ _ _ _ < /proc/$pid/stat 2>/dev/null || { u3=$last_u; s3=$last_s; }
total_cpu=$(echo "($u3 + $s3 - $u1 - $s1) / $HZ / $secs * 100" | awk '{printf "%.0f", $1}')
echo "avg_cpu_pct=$total_cpu peak_cpu_pct=$peak"
