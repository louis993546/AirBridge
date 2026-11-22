#!/usr/bin/env bash
set -euo pipefail

# One-time USB flash helper that installs the dual-OTA partition table (`partitions.csv`)
# AND ESP-IDF's rollback-enabled bootloader (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`).
# After flashing once over USB, use `./ota.sh` for all future wireless updates!

cd "$(dirname "$0")"

# Optional local ESP-IDF tools directory override from gitignored cfg.toml
if [ -z "${ESP_IDF_TOOLS_INSTALL_DIR:-}" ] && [ -f cfg.toml ]; then
  CFG_IDF_DIR=$(awk -F= '/^[[:space:]]*ESP_IDF_TOOLS_INSTALL_DIR[[:space:]]*=/{sub(/^[^=]*=/,""); gsub(/^[[:space:]]*"?|"?[[:space:]]*$/,""); print}' cfg.toml || true)
  if [ -n "${CFG_IDF_DIR}" ]; then
    export ESP_IDF_TOOLS_INSTALL_DIR="${CFG_IDF_DIR}"
  fi
fi

echo "==> Building release firmware..."
RUSTC_BOOTSTRAP=1 cargo build --release

ELF="target/riscv32imafc-esp-espidf/release/p4-airbridge"
BOOTLOADER=$(find target/riscv32imafc-esp-espidf/release/build/esp-idf-sys -path "*/out/build/bootloader/bootloader.bin" -print -quit 2>/dev/null || true)

EXTRA_ARGS=(--chip esp32p4 --flash-size 16mb --partition-table partitions.csv)
if [ -n "${BOOTLOADER}" ] && [ -f "${BOOTLOADER}" ]; then
  echo "==> Using rollback-enabled ESP-IDF bootloader: ${BOOTLOADER}"
  EXTRA_ARGS+=(--bootloader "${BOOTLOADER}")
fi

echo "==> Flashing over USB (with monitor)..."
espflash flash --monitor "${EXTRA_ARGS[@]}" "${ELF}" "$@"
