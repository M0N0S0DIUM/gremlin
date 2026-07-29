#!/usr/bin/env bash
# setup-autostart.sh — configure Gremlin to start automatically on login.
#
# Installs:  gremlin + sprite-viewer binaries → ~/.cargo/bin
#            sprite assets → ~/.local/share/gremlin/assets/sprites
# Daemon:    systemd user service (auto-restarts, ordered after network)
# Sprite:    Hyprland exec-once (retries daemon connection for 60s)
# Window:    Hyprland windowrule (float/pin/noborder)
#
# Idempotent — safe to run repeatedly. Pass --dry-run to preview.

set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
DRY_RUN=false
SCALE="${GREMLIN_SPRITE_SCALE:-4}"
for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=true ;;
        --scale=*) SCALE="${arg#--scale=}" ;;
    esac
done

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="${HOME}/.cargo/bin"
DATA_DIR="${HOME}/.local/share/gremlin"
HYPR_CONF="${HOME}/.config/hypr/hyprland.conf"
WINDOWRULE='windowrule = match:class ^(gremlin-sprite)$, float on, pin on, noborder on, noshadow on, nofocus on, noanim on, size 192 192, move 100%-220 100%-220'
EXEC_ONCE="exec-once = ${BIN_DIR}/sprite-viewer ${SCALE}"

ok()  { echo -e "${GREEN}✓${NC} $1"; }
warn(){ echo -e "${YELLOW}⚠${NC} $1"; }
err() { echo -e "${RED}✗${NC} $1"; exit 1; }
run() { $DRY_RUN || "$@"; }

echo "=== Gremlin autostart setup ==="
$DRY_RUN && echo "(dry run — no changes)"

# ── 0. Sanity: repo has what we need ──
[ -f "$REPO/Cargo.toml" ] || err "run from the gremlin repo: ./scripts/setup-autostart.sh"
[ -f "$REPO/assets/sprites/sprite-sheet-full.png" ] || err "sprite assets missing in repo"

# ── 1. Build + install binaries ──
echo "Building release binaries..."
run bash -c "cd '$REPO' && cargo build --release" || err "build failed"
run mkdir -p "$BIN_DIR"
run cp -f "$REPO/target/release/gremlin" "$BIN_DIR/"
run cp -f "$REPO/target/release/sprite-viewer" "$BIN_DIR/"
ok "binaries → ${BIN_DIR}/{gremlin,sprite-viewer}"

# ── 2. Install sprite assets where the daemon finds them from systemd ──
#     (daemon resolves: cwd → ~/.local/share/gremlin/<path> → exe dir)
run mkdir -p "$DATA_DIR/assets/sprites"
run cp -f "$REPO/assets/sprites/sprite-sheet-full.png" \
          "$REPO/assets/sprites/sprite-sheet-frame-map.json" \
          "$DATA_DIR/assets/sprites/"
ok "sprite assets → ${DATA_DIR}/assets/sprites"

# ── 3. Config exists? ──
if [ ! -f "${HOME}/.config/gremlin/config.toml" ]; then
    echo "Initializing config..."
    run "$BIN_DIR/gremlin" init || warn "gremlin init failed — run manually"
    ok "config created (~/.config/gremlin/config.toml)"
else
    ok "config exists"
fi

# ── 4. systemd daemon service ──
if systemctl --user is-enabled gremlin.service &>/dev/null; then
    ok "gremlin.service already enabled — restarting to pick up new binary"
    run systemctl --user restart gremlin.service
else
    echo "Installing gremlin systemd service..."
    run "$BIN_DIR/gremlin" service install || err "gremlin service install failed"
    ok "gremlin.service installed + started"
fi

# ── 5. Hyprland windowrule + exec-once ──
if [ ! -f "$HYPR_CONF" ]; then
    warn "${HYPR_CONF} not found — add manually:"
    echo "    $WINDOWRULE"
    echo "    $EXEC_ONCE"
else
    if grep -qF 'gremlin-sprite' "$HYPR_CONF"; then
        ok "windowrule already in hyprland.conf"
    else
        run bash -c "printf '\n# Gremlin sprite — borderless floating mascot\n%s\n' '$WINDOWRULE' >> '$HYPR_CONF'"
        ok "windowrule added"
    fi
    if grep -qF 'sprite-viewer' "$HYPR_CONF"; then
        ok "exec-once already in hyprland.conf"
    else
        run bash -c "printf '%s\n' '$EXEC_ONCE' >> '$HYPR_CONF'"
        ok "exec-once added"
    fi
fi

echo ""
echo "=== Done ==="
echo "Daemon:   systemctl --user status gremlin"
echo "Logs:     journalctl --user -u gremlin -f"
echo "Sprite:   hyprctl reload   (or re-login) — viewer waits up to 60s for the daemon"
echo "Test:     gremlin ask \"hello\""

$DRY_RUN && { echo ""; warn "dry run — no changes made"; }
exit 0