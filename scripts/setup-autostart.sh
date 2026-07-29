#!/usr/bin/env bash
# setup-autostart.sh — configure Gremlin to start automatically on login.
#
# Daemon:   systemd user service (survives logouts, starts before Hyprland)
# Sprite:   Hyprland exec-once (needs Wayland session)
# Window:   Hyprland windowrule (float/pin/noborder)
#
# Safe to run multiple times — idempotent. Pass --dry-run to preview.

set -euo pipefail

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
DRY_RUN=false; [[ "${1:-}" == "--dry-run" ]] && DRY_RUN=true

HYPR_CONF="${HOME}/.config/hypr/hyprland.conf"
SPRITE_VIEWER="${HOME}/.cargo/bin/sprite-viewer"
WINDOWRULE='windowrule = match:class ^(gremlin-sprite)$, float on, pin on, noborder on, noshadow on, nofocus on, noanim on, size 192 192, move 100%-220 100%-220'
EXEC_ONCE="exec-once = ${SPRITE_VIEWER} 4"

ok()  { echo -e "${GREEN}✓${NC} $1"; }
warn(){ echo -e "${YELLOW}⚠${NC} $1"; }
err() { echo -e "${RED}✗${NC} $1"; exit 1; }

echo "=== Gremlin autostart setup ==="
$DRY_RUN && echo "(dry run — no changes)"

# ── 1. Build binaries if missing ──
REPO="$(cd "$(dirname "$0")/.." && pwd)"
if [ ! -x "${SPRITE_VIEWER}" ]; then
    echo "Building sprite-viewer..."
    if ! $DRY_RUN; then
        (cd "$REPO" && cargo build --release --bin sprite-viewer) || err "build failed"
        mkdir -p "$(dirname "$SPRITE_VIEWER")"
        cp "$REPO/target/release/sprite-viewer" "$SPRITE_VIEWER"
    fi
    ok "sprite-viewer → ${SPRITE_VIEWER}"
else
    ok "sprite-viewer found at ${SPRITE_VIEWER}"
fi

# ── 2. systemd daemon service ──
if systemctl --user is-enabled gremlin.service &>/dev/null; then
    ok "gremlin.service already enabled"
else
    echo "Installing gremlin systemd service..."
    if ! $DRY_RUN; then
        gremlin service install || err "gremlin service install failed"
    fi
    ok "gremlin.service installed + enabled"
fi

# ── 3. Hyprland windowrule ──
if [ ! -f "$HYPR_CONF" ]; then
    warn "${HYPR_CONF} not found — skipping Hyprland rules"
else
    if grep -qF 'gremlin-sprite' "$HYPR_CONF"; then
        ok "windowrule already in hyprland.conf"
    else
        echo "Adding windowrule to hyprland.conf..."
        if ! $DRY_RUN; then
            echo "" >> "$HYPR_CONF"
            echo "# Gremlin sprite — borderless floating mascot" >> "$HYPR_CONF"
            echo "$WINDOWRULE" >> "$HYPR_CONF"
        fi
        ok "windowrule added"
    fi
fi

# ── 4. Hyprland exec-once for sprite-viewer ──
if [ ! -f "$HYPR_CONF" ]; then
    : # already warned
elif grep -qF 'sprite-viewer' "$HYPR_CONF"; then
    ok "exec-once for sprite-viewer already in hyprland.conf"
else
    echo "Adding exec-once to hyprland.conf..."
    if ! $DRY_RUN; then
        echo "$EXEC_ONCE" >> "$HYPR_CONF"
    fi
    ok "exec-once added"
fi

echo ""
echo "=== Done ==="
echo "Daemon:  systemctl --user status gremlin"
echo "Sprite:  starts with Hyprland (reload or re-login)"
echo "Logs:    journalctl --user -u gremlin -f"

if $DRY_RUN; then
    echo ""
    warn "dry run — no changes made. Run without --dry-run to apply."
fi