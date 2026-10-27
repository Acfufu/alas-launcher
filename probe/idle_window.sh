#!/usr/bin/env bash
# Idle-window experiment harness (direction-4, 2026-10-28 decision: 空闲间隙
# 自动模式 — run experiments ONLY when the farm has nothing to do, snapshot
# configs first, restore in a finally, never run with MaaEnd present).
#
# Gates (ALL must pass before the experiment command runs):
#   1. MaaEnd absent (ps) — the scan-vid mutex rule.
#   2. No past-due task: every enabled Scheduler.NextRun in every config
#      <instance>.json is in the future, AND the nearest one leaves enough
#      headroom for the experiment (--margin, default 15 min).
#   3. Config snapshot taken (md5-recorded) before anything runs.
#
# Finally: restore the snapshot (md5-verified), even if the experiment
# crashed the emulator. NOTE under tier-B (default) a crashed AVD stays
# down — pull it back manually (2026-10-28 user coordination item).
#
# Usage:
#   idle_window.sh [--payload DIR] [--margin MIN] -- <experiment command...>
#   idle_window.sh --dry-run ...   show gate results, run nothing
set -euo pipefail

PAYLOAD="${PAYLOAD:-/Applications/AzurLaneAutoScript.app/Contents/AzurLaneAutoScript}"
MARGIN_MIN=15
DRY_RUN=0
CMD=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --payload) PAYLOAD="$2"; shift 2 ;;
    --margin)  MARGIN_MIN="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --) shift; CMD=("$@"); break ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

log() { printf '\033[1;36m[idle-window]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[idle-window]\033[0m %s\n' "$*" >&2; exit 1; }

[[ -d "$PAYLOAD/config" ]] || die "payload config dir not found: $PAYLOAD/config"

# --- Gate 1: MaaEnd absent --------------------------------------------------
if pgrep -f "/Applications/MaaEnd.app/" >/dev/null 2>&1; then
  die "gate 1 FAILED: MaaEnd is running — the scan-vid mutex rule forbids experiments now"
fi
log "gate 1 ok: MaaEnd absent"

# --- Gate 2: idle window ----------------------------------------------------
# Every enabled task's NextRun must be in the future with >= margin headroom.
GATE2=$(python3 - "$PAYLOAD/config" "$MARGIN_MIN" <<'PY'
import glob, json, os, sys
from datetime import datetime, timedelta

cfg_dir, margin = sys.argv[1], int(sys.argv[2])
now = datetime.now()
horizon = now + timedelta(minutes=margin)
offenders = []
nearest = None
for path in glob.glob(os.path.join(cfg_dir, "*.json")):
    name = os.path.basename(path)
    if name.startswith("template"):
        continue  # templates carry placeholder 2023 dates, not real schedules
    try:
        data = json.load(open(path, encoding="utf-8"))
    except Exception:
        continue
    for task, body in data.items():
        sched = body.get("Scheduler") if isinstance(body, dict) else None
        if not isinstance(sched, dict) or sched.get("Enable") is not True:
            continue
        nxt = sched.get("NextRun")
        if not isinstance(nxt, str) or not nxt:
            continue
        try:
            t = datetime.strptime(nxt, "%Y-%m-%d %H:%M:%S")
        except ValueError:
            continue
        if nearest is None or t < nearest[0]:
            nearest = (t, f"{name}:{task}")
        if t <= horizon:
            offenders.append(f"{name}:{task} @ {nxt}")
if offenders:
    print("BUSY " + "; ".join(offenders[:6]))
else:
    if nearest:
        remaining = nearest[0] - now
        mins = int(remaining.total_seconds() // 60)
        print(f"IDLE next={nearest[1]} in ~{mins}min (margin {margin}min satisfied)")
    else:
        print("IDLE no-scheduled-tasks")
PY
)
case "$GATE2" in
  BUSY*) die "gate 2 FAILED: past-due or near tasks — $GATE2" ;;
  *)     log "gate 2 ok: $GATE2" ;;
esac

if [[ "$DRY_RUN" -eq 1 ]]; then
  log "dry-run: gates passed, not executing anything"
  exit 0
fi

# --- Gate 3: snapshot -------------------------------------------------------
STAMP="$(date +%Y%m%d-%H%M%S)"
SNAP="$PAYLOAD/config/.idle-exp-snapshot-$STAMP"
mkdir -p "$SNAP"
for f in "$PAYLOAD/config/"*.json; do
  [[ "$(basename "$f")" == template* ]] && continue
  cp "$f" "$SNAP/"
done
python3 - "$SNAP" <<'PY'
import hashlib, os, sys
snap = sys.argv[1]
lines = []
for name in sorted(os.listdir(snap)):
    if not name.endswith(".json"):
        continue
    lines.append(f"{hashlib.md5(open(os.path.join(snap, name), 'rb').read()).hexdigest()}  {name}")
open(os.path.join(snap, "md5sums"), "w").write("\n".join(lines) + "\n")
PY
log "gate 3 ok: snapshot at $SNAP ($(ls "$SNAP" | grep -c '\.json$') config files)"

restore() {
  log "restoring config snapshot…"
  for f in "$SNAP/"*.json; do
    cp "$f" "$PAYLOAD/config/"
  done
  python3 - "$SNAP" "$PAYLOAD/config" <<'PY'
import hashlib, os, sys
snap, cfg = sys.argv[1], sys.argv[2]
expected = dict(line.split("  ", 1)[::-1] for line in open(os.path.join(snap, "md5sums")).read().splitlines() if line)
mismatch = [n for n, h in expected.items()
            if hashlib.md5(open(os.path.join(cfg, n), "rb").read()).hexdigest() != h]
print("[idle-window] restore verified (md5 parity)" if not mismatch
      else f"[idle-window] WARN md5 mismatch after restore: {mismatch}")
PY
}
trap restore EXIT

if [[ ${#CMD[@]} -eq 0 ]]; then
  die "no experiment command given (pass -- <cmd...>)"
fi

log "running experiment: ${CMD[*]}"
set +e
"${CMD[@]}"
RC=$?
set -e
if [[ $RC -ne 0 ]]; then
  log "experiment exited rc=$RC"
  log "NOTE: if it crashed the AVD, tier-B does NOT auto-revive — start it manually (menu/webapp) and check ~/Library/Logs/DiagnosticReports/ for a fresh .ips"
fi
log "experiment phase done (rc=$RC)"
exit $RC
