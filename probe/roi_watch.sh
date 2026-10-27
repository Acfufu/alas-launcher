#!/bin/bash
# ROI profiling watcher (runs as root via osascript admin).
# Polls ALAS app python processes; when one sustains high CPU (a real task
# is running template matching / OCR), records a 120s py-spy profile of it.
# One recording per invocation, then exits.
OUT=/Users/acfufu/Codehub/alas-launcher/probe/out
PYSPY=/Users/acfufu/.local/bin/py-spy

log() { echo "$(date +%H:%M:%S) $*" >> "$OUT/roi_watch.log"; }

log "watcher start (max wait 40min, need 2 consecutive polls >20% cpu)"

TARGET=""
for i in $(seq 1 800); do
  BEST_PID=""; BEST_CPU=0
  for pid in $(pgrep -f "AzurLaneAutoScript/.venv/bin/python" 2>/dev/null); do
    cpu=$(ps -p "$pid" -o %cpu= 2>/dev/null | tr -d ' ')
    [ -z "$cpu" ] && continue
    log "poll pid=$pid cpu=$cpu"
    if awk "BEGIN{exit !($cpu > $BEST_CPU)}"; then BEST_PID=$pid; BEST_CPU=$cpu; fi
  done
  if [ -n "$BEST_PID" ] && awk "BEGIN{exit !($BEST_CPU > 20)}"; then
    # confirm on second consecutive poll
    sleep 3
    cpu2=$(ps -p "$BEST_PID" -o %cpu= 2>/dev/null | tr -d ' ')
    if [ -n "$cpu2" ] && awk "BEGIN{exit !($cpu2 > 20)}"; then
      TARGET=$BEST_PID
      break
    fi
  fi
  sleep 3
done

if [ -z "$TARGET" ]; then
  log "TIMEOUT: no task activity detected in 40min, exiting"
  exit 1
fi

log "ACTIVITY confirmed pid=$TARGET cpu=$BEST_CPU — recording 120s (nonblocking, 50Hz)"
"$PYSPY" record --pid "$TARGET" --duration 120 --rate 50 --nonblocking \
  --format raw -o "$OUT/roi_profile.txt" >> "$OUT/roi_watch.log" 2>&1
"$PYSPY" dump --pid "$TARGET" >| "$OUT/roi_dump.txt" 2>> "$OUT/roi_watch.log"

# also dump the other app pythons for context
for pid in $(pgrep -f "AzurLaneAutoScript/.venv/bin/python" 2>/dev/null); do
  [ "$pid" = "$TARGET" ] && continue
  "$PYSPY" dump --pid "$pid" >> "$OUT/roi_dump_others.txt" 2>&1
done

chmod 644 "$OUT"/roi_profile.txt "$OUT"/roi_dump.txt "$OUT"/roi_dump_others.txt "$OUT"/roi_watch.log 2>/dev/null
log "recording done, outputs in $OUT"
