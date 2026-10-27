#!/usr/bin/env bash
# Canary watcher (direction-6, 2026-10-28 decision: 触发式 — zero cost most
# weeks, surface ONLY when a fix entry names our crash families or a
# next-generation emulator preview appears).
#
# Watches three sources:
#   1. gfxstream commits (GitHub mirror, active) — new commits since the
#      last run whose message mentions our keywords.
#   2. Android Studio release notes (static page) — keyword diff vs the
#      last run's snapshot hash.
#   3. Android Studio preview page (static) — next-gen emulator wording.
#
# Keywords cover the four crash families from the 2026-10 forensics:
#   Metal / AGX / MoltenVK / device lost / GuestAngle / GPU timeout / kIOGPU
#
# Output: "CLEAN" (one line) or "HIT:" lines with what matched and where —
# a HIT means: tell the user, get the nod, THEN download a canary emulator.
#
# State: probe/out/canary-state/ (gfxstream watermark SHA + page hashes).
# First run records watermarks and reports nothing (baseline).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STATE_DIR="${CANARY_STATE_DIR:-$ROOT/probe/out/canary-state}"
mkdir -p "$STATE_DIR"

KEYWORDS='[Mm]etal|AGX|[Mm]oltenVK|[Dd]evice.?[Ll]ost|GuestAngle|GPU timeout|kIOGPU|MTLCommandBuffer|vkQueueSubmit'

log() { printf '%s\n' "$*"; }
hit()  { printf 'HIT: %s\n' "$*"; HITS=$((HITS+1)); }
HITS=0

# --- 1. gfxstream commits ----------------------------------------------------
# The mirror has no GitHub releases; commits are the fix signal.
COMMITS_FILE="$STATE_DIR/gfxstream-commits.json"
curl -sf "https://api.github.com/repos/google/gfxstream/commits?per_page=100" -o "$COMMITS_FILE"
if [[ $? -eq 0 && -s "$COMMITS_FILE" ]]; then
  WATERMARK_FILE="$STATE_DIR/gfxstream-watermark"
  if [[ ! -f "$WATERMARK_FILE" ]]; then
    python3 -c "import json;print(json.load(open('$COMMITS_FILE'))[0]['sha'])" > "$WATERMARK_FILE" 2>/dev/null
    log "(baseline) gfxstream watermark recorded"
  else
    WATERMARK="$(cat "$WATERMARK_FILE")"
    MATCHES=$(python3 - "$COMMITS_FILE" "$WATERMARK" "$KEYWORDS" <<'PY'
import json, re, sys
commits, watermark, kw = sys.argv[1], sys.argv[2], sys.argv[3]
pat = re.compile(kw)
seen_old = False
for c in json.load(open(commits)):
    if c["sha"] == watermark:
        seen_old = True
        break
    msg = c["commit"]["message"]
    if pat.search(msg):
        first = msg.split("\n")[0][:90]
        print(f"gfxstream commit {c['sha'][:9]}: {first}")
PY
)
    if [[ -n "$MATCHES" ]]; then
      while IFS= read -r line; do hit "$line"; done <<< "$MATCHES"
    fi
    # Advance the watermark regardless (already-reviewed commits).
    python3 -c "import json;print(json.load(open('$COMMITS_FILE'))[0]['sha'])" > "$WATERMARK_FILE" 2>/dev/null
    [[ -z "${SEEN_OLD:-}" ]] && :
  fi
else
  log "WARN: gfxstream commits fetch failed (network?)"
fi

# --- 2/3. static Android pages: keyword-diff vs snapshot ---------------------
check_page() {
  local name="$1" url="$2"
  local tmp="$STATE_DIR/$name.tmp" snap="$STATE_DIR/$name.snapshot"
  curl -sfL "$url" -o "$tmp" || { log "WARN: $name fetch failed"; return; }
  local body
  body="$(python3 - "$tmp" <<'PY'
import html, re, sys
text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
text = re.sub(r"<(script|style)[^>]*>.*?</\1>", " ", text, flags=re.S | re.I)
text = re.sub(r"<[^>]+>", " ", text)
print(re.sub(r"\s+", " ", html.unescape(text)))
PY
)"
  local newhash
  newhash="$(printf '%s' "$body" | shasum | cut -d' ' -f1)"
  if [[ ! -f "$snap" ]]; then
    printf '%s' "$newhash" > "$snap"
    log "(baseline) $name snapshot recorded"
    return
  fi
  if [[ "$(cat "$snap")" == "$newhash" ]]; then
    return # unchanged — nothing new to inspect
  fi
  # Page changed: report keyword hits in the FULL body (release-note
  # sections are small; a changed page plus a keyword = worth a look).
  local found
  found="$(printf '%s' "$body" | grep -ioE "$KEYWORDS" | sort -u | tr '\n' ',' )"
  if [[ -n "$found" ]]; then
    hit "$name changed and mentions: ${found%,} — review $url"
  else
    log "note: $name changed without our keywords ($url)"
  fi
  printf '%s' "$newhash" > "$snap"
}

check_page studio-releases "https://developer.android.com/studio/releases"
check_page studio-preview  "https://developer.android.com/studio/preview"

if [[ "$HITS" -eq 0 ]]; then
  log "CLEAN — no canary-worthy entries ($(date '+%F %T'))"
  exit 0
fi
log "→ $HITS hit(s). Action per direction-6: show the user, get the nod, then fetch the canary (5-10 GB)."
exit 1
