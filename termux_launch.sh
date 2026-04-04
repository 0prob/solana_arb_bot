#!/data/data/com.termux/files/usr/bin/bash
# ═══════════════════════════════════════════════════════════════════════════════
#  termux_launch.sh — Mobile-optimized launch script for solana_arb_bot
#
#  Designed for: Google Pixel 9 (Snapdragon 8 Gen 3), Termux + proot-distro
#  Debian, 12–16 GB RAM, Android 15+
#
#  Usage (inside Termux, NOT inside proot):
#    chmod +x termux_launch.sh
#    ./termux_launch.sh
#
#  The script:
#    1. Checks battery level and refuses to start below 20%.
#    2. Sets up a 2 GB swap file if not already present (prevents OOM kill).
#    3. Launches the bot inside proot-distro Debian with:
#       - nice -n 10 (lower CPU priority vs. Android system processes)
#       - ionice -c 3 (idle I/O class, prevents I/O starvation of Android)
#       - MALLOC_ARENA_MAX=2 (reduces glibc malloc fragmentation)
#       - RUST_LOG=sb=info,warn (minimal logging)
#    4. Restarts the bot automatically on crash (with 10-second backoff).
#
#  Prerequisites (run once in Termux):
#    pkg install proot-distro termux-api
#    proot-distro install debian
#    # Copy the bot binary into the proot environment:
#    cp /path/to/sb ~/debian/root/sb
#    cp /path/to/.env ~/debian/root/.env
# ═══════════════════════════════════════════════════════════════════════════════

set -euo pipefail

PROOT_DISTRO="debian"
BOT_PATH="/root/sb"
ENV_FILE="/root/.env"
LOG_FILE="/root/sb.log"
SWAP_FILE="$HOME/swapfile"
SWAP_SIZE_MB=2048

# ── 1. Battery check ──────────────────────────────────────────────────────────
check_battery() {
    if command -v termux-battery-status &>/dev/null; then
        local level
        level=$(termux-battery-status 2>/dev/null | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('percentage',100))" 2>/dev/null || echo 100)
        if [ "$level" -lt 20 ]; then
            echo "[WARN] Battery at ${level}% — waiting for charge above 20%..."
            while [ "$level" -lt 20 ]; do
                sleep 60
                level=$(termux-battery-status 2>/dev/null | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('percentage',100))" 2>/dev/null || echo 100)
            done
            echo "[INFO] Battery at ${level}% — starting bot."
        else
            echo "[INFO] Battery at ${level}% — OK."
        fi
    else
        echo "[INFO] termux-battery-status not available, skipping battery check."
    fi
}

# ── 2. Swap setup ─────────────────────────────────────────────────────────────
setup_swap() {
    if [ -f "$SWAP_FILE" ]; then
        echo "[INFO] Swap file already exists at $SWAP_FILE."
        return
    fi
    echo "[INFO] Creating ${SWAP_SIZE_MB} MB swap file at $SWAP_FILE..."
    # Use dd with bs=1M for compatibility with Termux's busybox.
    dd if=/dev/zero of="$SWAP_FILE" bs=1M count="$SWAP_SIZE_MB" status=progress
    chmod 600 "$SWAP_FILE"
    mkswap "$SWAP_FILE"
    echo "[INFO] Swap file created. To activate: swapon $SWAP_FILE"
    echo "[NOTE] swapon requires root or a kernel with CONFIG_SWAP=y."
    echo "       On non-rooted Pixel 9, swap may not be activatable."
    echo "       The swap file is still created for future use."
}

# ── 3. Launch bot inside proot ────────────────────────────────────────────────
launch_bot() {
    echo "[INFO] Launching solana_arb_bot inside proot-distro ${PROOT_DISTRO}..."

    # Environment variables for mobile-optimized operation.
    local env_vars=(
        "RUST_LOG=sb=info,warn"
        "MALLOC_ARENA_MAX=2"
        "MALLOC_MMAP_THRESHOLD_=131072"
        "MALLOC_TRIM_THRESHOLD_=131072"
        "NO_TUI=true"
        "SKIP_SIMULATION=true"
        "MAX_MEMORY_MB=2500"
        "SCANNER_MAX_CONCURRENCY=4"
        "TUI_FPS=5"
    )

    # Build env string for proot exec.
    local env_str=""
    for var in "${env_vars[@]}"; do
        env_str="$env_str $var"
    done

    # Launch with nice (lower CPU priority) and ionice (idle I/O).
    # proot-distro exec runs the command inside the Debian chroot.
    proot-distro login "$PROOT_DISTRO" -- \
        env $env_str \
        nice -n 10 \
        ionice -c 3 \
        "$BOT_PATH" \
            --no-tui \
            2>&1 | tee -a "$SWAP_FILE/../sb.log"
}

# ── 4. Auto-restart loop ──────────────────────────────────────────────────────
main() {
    check_battery
    setup_swap

    local backoff=10
    while true; do
        echo "[$(date '+%Y-%m-%d %H:%M:%S')] Starting bot (backoff=${backoff}s)..."
        if launch_bot; then
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] Bot exited cleanly."
            break
        else
            echo "[$(date '+%Y-%m-%d %H:%M:%S')] Bot crashed. Restarting in ${backoff}s..."
            sleep "$backoff"
            backoff=$((backoff < 120 ? backoff * 2 : 120))
        fi
    done
}

main "$@"
