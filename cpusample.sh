#!/bin/bash
# Sample cumulative CPU% of a process for a wall duration.
# Usage: cpusample.sh <pid> <seconds>
#
# NOTE: /proc/<pid>/stat field 2 is the comm in parentheses and may contain
# spaces, so everything after the LAST ')' is what we index. Fields after it
# are 0-based here: [11]=utime [12]=stime.
pid=$1
secs=$2
HZ=$(getconf CLK_TCK)

cpu_secs() {
  awk -v hz="$HZ" '{ s = $0; sub(/^.*\) /, "", s); split(s, f, " "); printf "%.6f", (f[12] + f[13]) / hz }' \
    "/proc/$1/stat" 2>/dev/null || echo ""
}

c0=$(cpu_secs "$pid") || exit 1
t0=$(date +%s.%N)
peak=0
last_c=$c0
last_t=$t0

while true; do
  now=$(date +%s.%N)
  el=$(awk -v a="$now" -v b="$t0" 'BEGIN{print (a-b>0)?a-b:0}')
  awk -v e="$el" -v s="$secs" 'BEGIN{exit !(e>s)}' && break
  c=$(cpu_secs "$pid")
  if [ -n "$c" ]; then
    dt=$(awk -v a="$now" -v b="$last_t" 'BEGIN{print a-b}')
    inst=$(awk -v c="$c" -v lc="$last_c" -v dt="$dt" 'BEGIN{if(dt>0) printf "%.0f", (c-lc)/dt*100; else print 0}')
    [ "$inst" -gt "$peak" ] && peak=$inst
    last_c=$c
    last_t=$now
  fi
  sleep 0.2
done

c1=$(cpu_secs "$pid")
[ -z "$c1" ] && c1=$last_c
total_cpu=$(awk -v a="$c1" -v b="$c0" -v s="$secs" 'BEGIN{printf "%.0f", (a-b)/s*100}')
echo "avg_cpu_pct=$total_cpu peak_cpu_pct=$peak"
