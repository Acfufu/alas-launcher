#!/bin/bash
# Phase driver for the power baseline. Run under the powermetrics window.
# Logs phase boundaries to phases.log and qemu CPU% to cpu_samples.log.
set -u

OUT=/Users/acfufu/Codehub/alas-launcher/probe/out
BENCH_PY=/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript/.venv/bin/python
ADB=/Users/acfufu/Library/Android/sdk/platform-tools/adb
PROBE=/Users/acfufu/Codehub/alas-launcher/probe
EMU_SERIAL=emulator-5554
PHASE_SECS=60

rm -f "$OUT"/phases.log "$OUT"/cpu_samples.log "$OUT"/bench_*.json "$OUT"/driver.log
QEMU_PID=$(pgrep -f "qemu-system-aarch64.*-avd Azurelane" | head -1)

log() { echo "$(date +%s.%N) $*" >> "$OUT/driver.log"; }

start_phase() {
  local name="$1"
  echo "$(date +%s.%N) PHASE_START $name" >> "$OUT/phases.log"
}

end_phase() {
  local name="$1" detail="$2"
  echo "$(date +%s.%N) PHASE_END $name $detail" >> "$OUT/phases.log"
}

# run_bench NAME SECONDS -- cmd...
run_bench() {
  local name="$1" secs="$2"; shift 2; shift # --
  local until_ep=$(( $(date +%s) + secs - 3 ))
  start_phase "$name"
  "$@" --until-epoch "$until_ep" --json-out "$OUT/bench_${name}.json" >| "$OUT/bench_${name}.stdout" 2>&1 &
  local bpid=$!
  local t_end=$(( $(date +%s) + secs ))
  while [ "$(date +%s)" -lt "$t_end" ]; do
    if [ -n "$QEMU_PID" ]; then
      local cpu; cpu=$(ps -p "$QEMU_PID" -o %cpu= 2>/dev/null | tr -d ' ')
      echo "$(date +%s.%N) qemu=${cpu:-NA}" >> "$OUT/cpu_samples.log"
    fi
    sleep 2
  done
  wait "$bpid" 2>/dev/null
  end_phase "$name" "pid=$bpid"
}

log "driver start, qemu_pid=$QEMU_PID"

# P0: idle reference — emulators + desktop only, no probe load
start_phase P0_idle
sleep "$PHASE_SECS"
end_phase P0_idle "-"

# P1: screenshot path as deployed (on-device PNG encode)
run_bench P1_screencap_png "$PHASE_SECS" -- "$BENCH_PY" "$PROBE/bench_screencap.py" --serial "$EMU_SERIAL" --mode png

# P2: OCR on CPU exactly as deployed (dynamic widths)
run_bench P2_ocr_cpu "$PHASE_SECS" -- "$BENCH_PY" "$PROBE/bench_ocr.py" --engine cpu --mode mixed

# P3: OCR via CoreML EP, dynamic widths (expected: fallback, today's shape policy)
run_bench P3_ocr_coreml_mixed "$PHASE_SECS" -- "$BENCH_PY" "$PROBE/bench_ocr.py" --engine coreml --mode mixed

# P4: OCR via CoreML EP, bucketed widths (the ANE lever)
run_bench P4_ocr_coreml_bucket "$PHASE_SECS" -- "$BENCH_PY" "$PROBE/bench_ocr.py" --engine coreml --mode bucket

# P5: template matching
run_bench P5_template "$PHASE_SECS" -- "$BENCH_PY" "$PROBE/bench_template.py"

# P6: combined pseudo-ALAS loop: screenshot + OCR(cpu) + template concurrently
start_phase P6_combined
u=$(( $(date +%s) + PHASE_SECS - 3 ))
"$BENCH_PY" "$PROBE/bench_screencap.py" --serial "$EMU_SERIAL" --mode png --until-epoch "$u" --json-out "$OUT/bench_P6_screencap.json" >| "$OUT/bench_P6_screencap.stdout" 2>&1 &
s1=$!
"$BENCH_PY" "$PROBE/bench_ocr.py" --engine cpu --mode mixed --until-epoch "$u" --json-out "$OUT/bench_P6_ocr.json" >| "$OUT/bench_P6_ocr.stdout" 2>&1 &
s2=$!
"$BENCH_PY" "$PROBE/bench_template.py" --until-epoch "$u" --json-out "$OUT/bench_P6_template.json" >| "$OUT/bench_P6_template.stdout" 2>&1 &
s3=$!
t_end=$(( $(date +%s) + PHASE_SECS ))
while [ "$(date +%s)" -lt "$t_end" ]; do
  if [ -n "$QEMU_PID" ]; then
    cpu=$(ps -p "$QEMU_PID" -o %cpu= 2>/dev/null | tr -d ' ')
    echo "$(date +%s.%N) qemu=${cpu:-NA}" >> "$OUT/cpu_samples.log"
  fi
  sleep 2
done
wait $s1 $s2 $s3 2>/dev/null
end_phase P6_combined "3-bench"

log "driver done"
echo "DRIVER_DONE $(date)" >> "$OUT/phases.log"
