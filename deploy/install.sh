#!/usr/bin/env bash
set -euo pipefail

# Install script for polybot systemd service.
# Run as root or with sudo.

INSTALL_DIR=/opt/polybot
SERVICE_USER=polybot

echo "==> Creating user and directories..."
id -u "$SERVICE_USER" &>/dev/null || useradd --system --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
mkdir -p "$INSTALL_DIR/data"
chown -R "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR"

echo "==> Building release binary..."
cargo build --release
cp target/release/polybot "$INSTALL_DIR/polybot"
chmod 755 "$INSTALL_DIR/polybot"

echo "==> Installing systemd service..."
cp deploy/polybot.service /etc/systemd/system/polybot.service
systemctl daemon-reload

if [ ! -f "$INSTALL_DIR/.env" ]; then
    cp deploy/.env.example "$INSTALL_DIR/.env"
    chmod 600 "$INSTALL_DIR/.env"
    chown "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR/.env"
    echo "==> Created $INSTALL_DIR/.env — edit it with your private key before starting."
else
    echo "==> $INSTALL_DIR/.env already exists, not overwriting."
fi

echo "==> Done. Start with: systemctl start polybot"
echo "    Enable on boot:   systemctl enable polybot"
echo "    View logs:         journalctl -u polybot -f"
