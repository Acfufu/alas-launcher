#!/bin/bash
# Root side of the baseline: run powermetrics around the phase driver.
# Invoked via osascript "with administrator privileges" so powermetrics (root)
# and the driver share one timeline. One password prompt covers everything.
set -u

OUT=/Users/acfufu/Codehub/alas-launcher/probe/out
PM_SAMPLE_MS=2000
PM_COUNT=240   # 480s >= 7 phases x 60s + settle + margin

echo "ADMIN_START $(date) uid=$(id -u)"
echo "PM_LAUNCH_EPOCH $(date +%s)" >| "$OUT/pm_meta.log"

powermetrics -i "$PM_SAMPLE_MS" -n "$PM_COUNT" \
  -s cpu_power,gpu_power,ane_power \
  --show-process-gpu \
  -f text -o "$OUT/pm.txt" &
PM_PID=$!
echo "$PM_PID" >| "$OUT/pm_pid"

sleep 3   # let the first sample settle before phases start

bash /Users/acfufu/Codehub/alas-launcher/probe/phase_driver.sh

wait "$PM_PID" 2>/dev/null
chmod 644 "$OUT"/pm.txt "$OUT"/phases.log "$OUT"/cpu_samples.log "$OUT"/pm_meta.log "$OUT"/driver.log "$OUT"/bench_*.json 2>/dev/null
echo "ADMIN_DONE $(date)"
