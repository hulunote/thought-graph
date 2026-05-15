#!/usr/bin/env bash
#
# Build a single distributable DMG containing:
#   - ThoughtGraph.app (the Tauri GUI)
#   - thoughtgraph-mcp (the MCP server), embedded inside the .app at
#     Contents/Resources/bin/thoughtgraph-mcp
#
# After installation (drag .app to /Applications), Claude Desktop can be
# pointed at /Applications/ThoughtGraph.app/Contents/Resources/bin/thoughtgraph-mcp.
#
# Usage:  ./scripts/build-dmg.sh

set -euo pipefail

# ---------------------------------------------------------------------------
# Locate repo root no matter where we're invoked from.
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

APP_NAME="ThoughtGraph"
VERSION="$(awk -F'"' '/^version =/ { print $2; exit }' src-tauri/Cargo.toml)"
ARCH="$(uname -m)"   # x86_64 or arm64
OUT_DIR="$ROOT/dist"
OUT_DMG="$OUT_DIR/${APP_NAME}-${VERSION}-${ARCH}.dmg"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m!! \033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Prereq checks.
# ---------------------------------------------------------------------------
command -v cargo  >/dev/null || fail "cargo not in PATH (install Rust)"
command -v npm    >/dev/null || fail "npm not in PATH (install Node.js)"
command -v hdiutil >/dev/null || fail "hdiutil not found — macOS only"

if [[ ! -d "$ROOT/node_modules/@tauri-apps/cli" ]]; then
  log "Installing npm dev deps (Tauri CLI)..."
  npm install
fi

# ---------------------------------------------------------------------------
# 1. Build the MCP server (release).
# ---------------------------------------------------------------------------
log "Building thoughtgraph-mcp (release)..."
cargo build -p thoughtgraph-mcp --release

MCP_BIN="$ROOT/target/release/thoughtgraph-mcp"
[[ -x "$MCP_BIN" ]] || fail "thoughtgraph-mcp binary not found at $MCP_BIN"

# ---------------------------------------------------------------------------
# 2. Build the Tauri .app. Tauri's bundler also wants to emit its own DMG;
#    skip that with --bundles app since we make our own.
# ---------------------------------------------------------------------------
log "Building ThoughtGraph.app (Tauri release)..."
npx tauri build --bundles app

APP_SRC="$ROOT/target/release/bundle/macos/${APP_NAME}.app"
[[ -d "$APP_SRC" ]] || fail "expected .app not found at $APP_SRC"

# ---------------------------------------------------------------------------
# 3. Inject MCP binary into the .app bundle.
# ---------------------------------------------------------------------------
log "Embedding thoughtgraph-mcp inside .app..."
RES_BIN_DIR="$APP_SRC/Contents/Resources/bin"
mkdir -p "$RES_BIN_DIR"
cp "$MCP_BIN" "$RES_BIN_DIR/thoughtgraph-mcp"
chmod +x "$RES_BIN_DIR/thoughtgraph-mcp"

# Strip quarantine attr (only present if downloaded via browser; harmless here).
xattr -dr com.apple.quarantine "$APP_SRC" 2>/dev/null || true

# Ad-hoc resign so the bundle is internally consistent after modification.
# (Tauri leaves an ad-hoc signature; modifying Resources breaks its seal.)
log "Ad-hoc re-signing the .app..."
codesign --force --deep --sign - "$APP_SRC"
codesign --verify --deep --strict "$APP_SRC" 2>&1 | sed 's/^/    /'

# ---------------------------------------------------------------------------
# 4. Stage DMG contents.
# ---------------------------------------------------------------------------
log "Staging DMG payload..."
STAGE="$(mktemp -d -t thoughtgraph-dmg)"
trap 'rm -rf "$STAGE"' EXIT

cp -R "$APP_SRC" "$STAGE/"
ln -s /Applications "$STAGE/Applications"

cat > "$STAGE/README.txt" <<EOF
ThoughtGraph ${VERSION} (${ARCH})
=================================

INSTALL
  1. Drag ThoughtGraph.app onto the Applications shortcut.

USE THE GUI
  Open ThoughtGraph from Launchpad / Spotlight.

CONNECT CLAUDE DESKTOP TO THE MCP MEMORY SERVER
  Edit (or create):
    ~/Library/Application Support/Claude/claude_desktop_config.json

  Add:
    {
      "mcpServers": {
        "thoughtgraph": {
          "command": "/Applications/ThoughtGraph.app/Contents/Resources/bin/thoughtgraph-mcp"
        }
      }
    }

  Restart Claude Desktop. The "thoughtgraph" server should show 13 tools.

NOTES
  - GUI and MCP server share the same SQLite database at
    ~/Library/Application Support/com.chanshunli.thoughtgraph/thoughtgraph.sqlite3
    (WAL mode — concurrent read/write is safe).
  - To use a separate DB for Claude only, add env to the MCP entry:
        "env": { "THOUGHTGRAPH_DB": "/absolute/path/to/your.sqlite3" }
  - GraphViz \`dot\` is required for PDF/PNG rendering:
        brew install graphviz
EOF

# ---------------------------------------------------------------------------
# 5. Build the DMG.
# ---------------------------------------------------------------------------
mkdir -p "$OUT_DIR"
rm -f "$OUT_DMG"

log "Creating DMG → $OUT_DMG"
hdiutil create \
  -volname "$APP_NAME ${VERSION}" \
  -srcfolder "$STAGE" \
  -ov -format UDZO \
  "$OUT_DMG" >/dev/null

# Sanity check.
log "Verifying DMG..."
hdiutil verify "$OUT_DMG" >/dev/null

SIZE="$(du -h "$OUT_DMG" | awk '{print $1}')"
log "Done.  ${OUT_DMG}  (${SIZE})"
log "Contents:"
hdiutil attach -nobrowse -readonly -plist "$OUT_DMG" >/dev/null
MOUNT_POINT="$(hdiutil info -plist | /usr/libexec/PlistBuddy -c 'Print :images' /dev/stdin 2>/dev/null | grep -A1 "mount-point" | tail -1 | xargs || true)"
# Best-effort listing (mount-point parsing is fragile, so just list /Volumes/<name>).
VOL_DIR="/Volumes/${APP_NAME} ${VERSION}"
if [[ -d "$VOL_DIR" ]]; then
  ls -la "$VOL_DIR" | sed 's/^/    /'
  hdiutil detach "$VOL_DIR" >/dev/null 2>&1 || true
fi
