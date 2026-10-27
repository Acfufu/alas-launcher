#!/usr/bin/env bash
# One-shot local bundle assembly (direction-7b, 2026-10-28 decision: local
# one-click script first; CI-ification deferred).
#
# Deployment facts this script encodes (see the memory files):
#   - webapp/dist is gitignored: pushing the payload repo NEVER ships the
#     frontend. The dist must be rsynced into the bundle's payload tree.
#   - The launcher itself fetch+resets the payload repo and re-injects the
#     control_api patch on every start — so payload CODE in the bundle is
#     self-updating; what only this shell can ship is (a) the launcher
#     binary and (b) the webapp dist.
#
# Modes:
#   --update [APP]   DEFAULT. Build launcher + webapp dist, then update the
#                    INSTALLED app in place: replace MacOS/alas-launcher,
#                    rsync dist (code only). config/, log/, .venv/, the
#                    payload git tree — all untouched. Restart the app to
#                    apply (the running instance keeps its old binary).
#   --full [DEST]    Assemble a complete fresh .app at DEST (default
#                    build/AzurLaneAutoScript.app) from the payload working
#                    clone (must be clean; it is synced EXCLUDING .venv —
#                    run `uv sync` inside the bundle after first launch, or
#                    pass --seed-venv-from=OLD_APP to transplant one).
#   --seed-venv-from=APP   (--full only) transplant .venv from an existing
#                    bundle instead of leaving it absent.
#
# The payload working clone (default ~/Codehub/alas-payload) must be clean;
# --update never touches it beyond building its webapp dist.
set -euo pipefail

LAUNCHER_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PAYLOAD_REPO="${PAYLOAD_REPO:-$HOME/Codehub/alas-payload}"
APP_DEFAULT="/Applications/AzurLaneAutoScript.app"
MODE="update"
DEST=""
SEED_VENV_FROM=""

for arg in "$@"; do
  case "$arg" in
    --update) MODE="update" ;;
    --full) MODE="full" ;;
    --seed-venv-from=*) SEED_VENV_FROM="${arg#*=}" ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *)
      if [[ -z "$DEST" && "$arg" != -* ]]; then DEST="$arg"
      else echo "unknown arg: $arg" >&2; exit 2
      fi
      ;;
  esac
done

log() { printf '\033[1;34m[assemble]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[assemble]\033[0m %s\n' "$*" >&2; exit 1; }

[[ -d "$PAYLOAD_REPO/.git" ]] || die "payload repo not found: $PAYLOAD_REPO (set PAYLOAD_REPO=...)"

# --- node toolchain: GUI/CI shells lack nvm on PATH (known trap) ----------
NODE_BIN=""
for candidate in "$(command -v node 2>/dev/null | xargs dirname 2>/dev/null)" \
  "$HOME/.nvm/versions/node/v26.12.1/bin" \
  "$HOME/.nvm/versions/node/v24.19.0/bin"; do
  if [[ -n "$candidate" && -x "$candidate/node" ]]; then NODE_BIN="$candidate"; break; fi
done
[[ -n "$NODE_BIN" ]] || die "node not found (nvm not on PATH and no ~/.nvm install)"
export PATH="$NODE_BIN:$PATH"

# --- 1. launcher binary -----------------------------------------------------
log "building launcher (cargo build --release)…"
( cd "$LAUNCHER_ROOT" && cargo build --release )
BIN="$LAUNCHER_ROOT/target/release/alas-launcher"
[[ -x "$BIN" ]] || die "built binary missing: $BIN"

# --- 2. webapp dist (the gitignored deliverable) ---------------------------
log "building webapp dist…"
if ! ( cd "$PAYLOAD_REPO" && git diff --quiet && git diff --cached --quiet ); then
  # Uncommitted changes are fine for a LOCAL assembly (that is the point),
  # but say so — the dist may contain unreleased code.
  log "note: payload working clone has uncommitted changes; dist will include them"
fi
( cd "$PAYLOAD_REPO/webapp" && npm run build )
DIST="$PAYLOAD_REPO/webapp/dist"
[[ -f "$DIST/index.html" ]] || die "webapp build produced no dist/index.html"

# --- 3. mode dispatch -------------------------------------------------------
md5_of() { md5 -q "$1"; }

if [[ "$MODE" == "update" ]]; then
  APP="${DEST:-$APP_DEFAULT}"
  [[ -d "$APP/Contents/MacOS" ]] || die "--update target is not an installed app: $APP"
  log "updating $APP in place (binary + dist only)…"
  if pgrep -x alas-launcher >/dev/null 2>&1; then
    log "NOTE: alas-launcher is running — quit it before relaunching to pick up the new binary"
  fi
  install -m 0755 "$BIN" "$APP/Contents/MacOS/alas-launcher"
  rsync -a --delete "$DIST/" "$APP/Contents/AzurLaneAutoScript/webapp/dist/"
  log "updated. verify with:"
  echo "  md5 -q \"$BIN\" vs $(md5_of "$APP/Contents/MacOS/alas-launcher")"
  echo "  ls \"$APP/Contents/AzurLaneAutoScript/webapp/dist/\""
else
  DEST="${DEST:-$LAUNCHER_ROOT/build/AzurLaneAutoScript.app}"
  [[ -e "$DEST" ]] && die "destination already exists (refusing to overwrite): $DEST"
  log "assembling fresh app at $DEST…"
  mkdir -p "$DEST/Contents/MacOS" "$DEST/Contents/Resources"
  install -m 0755 "$BIN" "$DEST/Contents/MacOS/alas-launcher"
  cp "$LAUNCHER_ROOT/icons/icon.icns" "$DEST/Contents/Resources/icon.icns"
  cat > "$DEST/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>alas-launcher</string>
    <key>CFBundleIdentifier</key><string>moe.taiho.alas-launcher</string>
    <key>CFBundleName</key><string>AzurLaneAutoScript</string>
    <key>CFBundleDisplayName</key><string>AzurLaneAutoScript</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
  # Payload tree: full clone state (the launcher re-fetches on start anyway).
  # .venv and log/ excluded; config rides along from the working clone.
  rsync -a \
    --exclude '.venv' \
    --exclude 'log/' \
    "$PAYLOAD_REPO/" "$DEST/Contents/AzurLaneAutoScript/"
  if [[ -n "$SEED_VENV_FROM" ]]; then
    [[ -d "$SEED_VENV_FROM/Contents/AzurLaneAutoScript/.venv" ]] \
      || die "--seed-venv-from has no .venv: $SEED_VENV_FROM"
    log "transplanting .venv from $SEED_VENV_FROM…"
    rsync -a "$SEED_VENV_FROM/Contents/AzurLaneAutoScript/.venv/" \
      "$DEST/Contents/AzurLaneAutoScript/.venv/"
  else
    log "NOTE: no .venv in the fresh bundle — first launch needs \`uv sync\` in Contents/AzurLaneAutoScript (or re-run with --seed-venv-from)"
  fi
  log "assembled: $DEST"
  du -sh "$DEST"
fi
