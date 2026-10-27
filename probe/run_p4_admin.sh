#!/bin/bash
# Root side of the P4 ANE revalidation: powermetrics around two static-N OCR
# phases. Same protocol as run_baseline_admin.sh but a separate *2 file set,
# so the 2026-10-27 morning baseline artifacts stay intact. One password
# prompt covers everything. Phases: 40s idle ref -> 60s staticn(bucket W)
# -> 60s staticn_w640(all crops padded to 640). Game must stay foreground.
set -u

OUT=/Users/acfufu/Codehub/alas-launcher/probe/out
PM_SAMPLE_MS=2000
PM_COUNT=90   # 180s >= 3s settle + 160s of phases + margin

rm -f "$OUT"/pm2.txt "$OUT"/pm2_pid "$OUT"/pm_meta2.log "$OUT"/phases2.log \
      "$OUT"/cpu2_samples.log "$OUT"/bench_P4s_*.json "$OUT"/bench_P4s_*.stdout

echo "ADMIN_START $(date) uid=$(id -u)"
echo "PM_LAUNCH_EPOCH $(date +%s)" >| "$OUT/pm_meta2.log"

powermetrics -i "$PM_SAMPLE_MS" -n "$PM_COUNT" \
  -s cpu_power,gpu_power,ane_power \
  -f text -o "$OUT/pm2.txt" &
PM_PID=$!
echo "$PM_PID" >| "$OUT/pm2_pid"

sleep 3   # let the first sample settle before phases start

BENCH_PY=/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript/.venv/bin/python
ADB=/Users/acfufu/Library/Android/sdk/platform-tools/adb
PROBE=/Users/acfufu/Codehub/alas-launcher/probe
EMU_SERIAL=emulator-5554
QEMU_PID=$(pgrep -f "qemu-system-aarch64.*-avd Azurelane" | head -1)

mark() { echo "$(date +%s.%N) $*" >> "$OUT/phases2.log"; }

run_phase() {
  local name="$1" secs="$2"; shift 2; shift  # consume name, secs, and the -- separator
  local until_ep=$(( $(date +%s) + secs - 3 ))
  mark "PHASE_START $name"
  "$@" --until-epoch "$until_ep" --json-out "$OUT/bench_${name}.json" >| "$OUT/bench_${name}.stdout" 2>&1 &
  local bpid=$!
  local t_end=$(( $(date +%s) + secs ))
  while [ "$(date +%s)" -lt "$t_end" ]; do
    if [ -n "$QEMU_PID" ]; then
      local cpu; cpu=$(ps -p "$QEMU_PID" -o %cpu= 2>/dev/null | tr -d ' ')
      echo "$(date +%s.%N) qemu=${cpu:-NA}" >> "$OUT/cpu2_samples.log"
    fi
    sleep 2
  done
  wait "$bpid" 2>/dev/null
  mark "PHASE_END $name"
}

mark "PHASE_START P0_idle"
sleep 40
mark "PHASE_END P0_idle"

run_phase P4s_staticn 60 -- "$BENCH_PY" "$PROBE/bench_ocr.py" --engine coreml --mode bucket --model "$OUT/azur_lane_staticn.onnx"

run_phase P4s_static640 60 -- "$BENCH_PY" "$PROBE/bench_ocr.py" --engine coreml --mode fixed640 --model "$OUT/azur_lane_staticn_w640.onnx"

wait "$PM_PID" 2>/dev/null
chmod 644 "$OUT"/pm2.txt "$OUT"/phases2.log "$OUT"/cpu2_samples.log "$OUT"/pm_meta2.log \
          "$OUT"/bench_P4s_*.json "$OUT"/bench_P4s_*.stdout 2>/dev/null
echo "ADMIN_DONE $(date)"
