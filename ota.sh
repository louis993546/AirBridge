#!/usr/bin/env bash
set -euo pipefail

# Wireless OTA build + deploy script for ESP32-P4 AirBridge.
# Usage:
#   ./ota.sh                         # uploads to p4-airbridge.local (or MDNS_HOSTNAME in cfg.toml)
#   ./ota.sh 192.168.1.153           # uploads to a specific IP/host

cd "$(dirname "$0")"

# Auto-reuse existing sibling ESP-IDF v5.5.3 install if present and not overridden
if [ -z "${ESP_IDF_TOOLS_INSTALL_DIR:-}" ] && [ -d "../esp32c6/hello-new-world/.embuild/espressif" ]; then
  export ESP_IDF_TOOLS_INSTALL_DIR="custom:../esp32c6/hello-new-world/.embuild/espressif"
fi

# Read optional MDNS_HOSTNAME and OTA_TOKEN from local cfg.toml
CFG_HOST=""
OTA_TOKEN=""
if [ -f cfg.toml ]; then
  CFG_HOST=$(awk -F= '/^[[:space:]]*MDNS_HOSTNAME[[:space:]]*=/{gsub(/["[:space:]]/,"",$2); print $2}' cfg.toml || true)
  OTA_TOKEN=$(awk -F= '/^[[:space:]]*OTA_TOKEN[[:space:]]*=/{sub(/^[^=]*=/,""); gsub(/^[[:space:]]*"?|"?[[:space:]]*$/,""); print}' cfg.toml || true)
fi

TARGET_HOST="${1:-${CFG_HOST:-p4-airbridge}.local}"
ELF="target/riscv32imafc-esp-espidf/release/p4-airbridge"
BIN="target/p4-airbridge-ota.bin"

echo "==> [1/3] Building release firmware..."
RUSTC_BOOTSTRAP=1 cargo build --release

echo "==> [2/3] Converting ELF -> ESP-IDF OTA binary (${BIN})..."
espflash save-image --chip esp32p4 --flash-size 16mb --min-chip-rev 3.1 "${ELF}" "${BIN}"
ls -lh "${BIN}"

echo "==> [3/3] Uploading OTA firmware to http://${TARGET_HOST}/ota ..."
AUTH_ARGS=()
if [ -n "${OTA_TOKEN}" ]; then
  AUTH_ARGS=(-H "X-OTA-Token: ${OTA_TOKEN}")
fi

curl --fail-with-body --progress-bar \
  -X POST \
  "${AUTH_ARGS[@]}" \
  --data-binary "@${BIN}" \
  "http://${TARGET_HOST}/ota"

echo ""
echo "Done! Board is rebooting into the new OTA slot (~6-10s)."
