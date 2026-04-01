#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────
# RICOZ SNIPER - VPS Deployment Script (Ubuntu)
# ─────────────────────────────────────────────────────────────
set -euo pipefail

APP_NAME="ricoz-sniper"
APP_DIR="/opt/${APP_NAME}"
APP_USER="ricoz"
SERVICE_FILE="/etc/systemd/system/${APP_NAME}.service"
LOGROTATE_FILE="/etc/logrotate.d/${APP_NAME}"
LOG_DIR="/var/log/${APP_NAME}"
DATA_DIR="${APP_DIR}/data"

echo "=== RICOZ SNIPER Deployment ==="
echo ""

# ── 1. System dependencies ─────────────────────────────────
echo "[1/7] Installing system dependencies..."
apt-get update -qq
apt-get install -y -qq build-essential pkg-config libssl-dev curl git

# ── 2. Install Rust (if not present) ───────────────────────
echo "[2/7] Checking Rust installation..."
if ! command -v rustc &>/dev/null; then
    echo "  Installing Rust via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
else
    echo "  Rust already installed: $(rustc --version)"
fi

# ── 3. Create application user and directories ─────────────
echo "[3/7] Setting up user and directories..."
if ! id "${APP_USER}" &>/dev/null; then
    useradd --system --create-home --shell /usr/sbin/nologin "${APP_USER}"
fi
mkdir -p "${APP_DIR}" "${LOG_DIR}" "${DATA_DIR}"

# ── 4. Build release binary ────────────────────────────────
echo "[4/7] Building release binary..."
if [ -d ".git" ]; then
    # We are in the source directory.
    cargo build --release
    cp "target/release/${APP_NAME}" "${APP_DIR}/${APP_NAME}"
else
    echo "  ERROR: Run this script from the project root directory."
    exit 1
fi

# Copy .env if it exists and the target does not yet have one.
if [ -f ".env" ] && [ ! -f "${APP_DIR}/.env" ]; then
    cp .env "${APP_DIR}/.env"
    chmod 600 "${APP_DIR}/.env"
fi

chown -R "${APP_USER}:${APP_USER}" "${APP_DIR}" "${LOG_DIR}"

# ── 5. Create systemd service ──────────────────────────────
echo "[5/7] Configuring systemd service..."
cat > "${SERVICE_FILE}" <<EOF
[Unit]
Description=RICOZ SNIPER - Solana Auto-Trading Sniper Bot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${APP_USER}
Group=${APP_USER}
WorkingDirectory=${APP_DIR}
ExecStart=${APP_DIR}/${APP_NAME}
EnvironmentFile=${APP_DIR}/.env
Restart=on-failure
RestartSec=10
StandardOutput=append:${LOG_DIR}/${APP_NAME}.log
StandardError=append:${LOG_DIR}/${APP_NAME}.log

# Security hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=${APP_DIR} ${LOG_DIR}
PrivateTmp=true

# Resource limits
LimitNOFILE=65536
MemoryMax=512M

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload

# ── 6. Configure logrotate ──────────────────────────────────
echo "[6/7] Configuring logrotate..."
cat > "${LOGROTATE_FILE}" <<EOF
${LOG_DIR}/*.log {
    daily
    rotate 14
    compress
    delaycompress
    missingok
    notifempty
    copytruncate
    maxsize 50M
}
EOF

# ── 7. Enable and start service ─────────────────────────────
echo "[7/7] Starting service..."
systemctl enable "${APP_NAME}"
systemctl restart "${APP_NAME}"

echo ""
echo "=== Deployment complete ==="
echo "  Status:  systemctl status ${APP_NAME}"
echo "  Logs:    journalctl -u ${APP_NAME} -f"
echo "  Log dir: ${LOG_DIR}"
echo "  Data:    ${DATA_DIR}"
