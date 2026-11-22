# AGENTS.md — Technical Reference for AI Coding Agents

This file documents the architecture, build constraints, and hard-won hardware/firmware gotchas in `p4-airbridge` so future maintenance sessions don't re-discover them.

---

## 1. Architecture Overview

`p4-airbridge` is an ESP-IDF (`std`) Rust firmware for **ESP32-P4** (rev v3.2, Waveshare ESP32-P4-Module-DEV-KIT) bridging a **USB IPP-USB (`07/01/04`) multifunction printer/scanner** onto **100M Ethernet** as a native **AirScan (eSCL)** + **AirPrint (IPP/2.0)** appliance.

| Module                          | Responsibility                                                                                                                                                                                                                                            |
| :------------------------------ | :-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `src/main.rs`                   | Boots NVS, Ethernet, USB Host, mDNS, port-80 `EspHttpServer` (Web UI, `/usb`, `/cal/set`, `/reboot`, `/eSCL/*`, Epson static assets `/icon.png`, `/PRESENTATION/*`, `/LANGUAGES/*`), and port-631 pure-Rust IPP server.                                   |
| `src/config.rs` + `build.rs`    | Loads optional compile-time overrides from gitignored `cfg.toml` (`AIRBRIDGE_*`), falling back to public-safe generic defaults when `cfg.toml` is absent.                                                                                                 |
| `src/eth.rs`                    | Configures ESP32-P4 internal EMAC + IP101GRI RMII PHY via raw `esp_idf_svc::sys::*` (`esp-idf-svc`'s high-level `eth.rs` is gated to original `esp32` only).                                                                                              |
| `src/wifi.rs`                   | Optional Wi-Fi 6 (WPA3-SAE, PMF required, `WIFI_PS_NONE`, background auto-reconnect) via onboard ESP32-C6 over SDIO (`espressif/esp_wifi_remote` + `espressif/esp_hosted`, default pins `CLK=18, CMD=19, D0..D3=14..17, RST=54`) when `WIFI_SSID` is set. |
| `src/mdns.rs`                   | Advertises `_uscan._tcp` (port 80), `_ipp._tcp` (port 631) + `_universal._sub._ipp._tcp` subtype via `mdns_service_subtype_add_for_host`, and `_http._tcp` (port 80).                                                                                     |
| `src/ota.rs`                    | Wireless OTA (`POST /ota`) streaming `.bin` into inactive `ota_0`/`ota_1` slot (`partitions.csv`), validating `0xE9` magic & optional `OTA_TOKEN`, and confirming rollback validity only after Ethernet/Wi-Fi DHCP succeeds.                              |
| `src/usb_host.rs`               | Installs ESP-IDF High-Speed USB 2.0 Host (`peripheral_map = 0`), runs lib + client event threads, enumerates descriptors, and spawns `ipp_usb_init` to activate all `07/01/04` interfaces.                                                                |
| `src/ipp_usb.rs`                | Thread-safe pool of `IppUsbChannel`s (up to 4 concurrent Bulk IN/OUT pairs). Streams HTTP/1.1 requests & responses (both `Content-Length` and `Transfer-Encoding: chunked`), rewrites `localhost` URLs, and drains stale Bulk IN data on error/cancel.    |
| `src/escl.rs`                   | Proxies `/eSCL/*` to `IppUsbBridge` (with `MockEpsonBridge` fallback when unplugged). Runs `ScanCalStore::rewrite_scan_settings()` on `POST /eSCL/ScanJobs`.                                                                                              |
| `src/scan_cal.rs`               | NVS-persisted (`airbridge` namespace) Epson hardware eSCL tone settings (`Brightness`, `Contrast`, `Sharpen`, `CompressionFactor`) injected into `ScanSettings` XML only if the client didn't explicitly send them.                                       |
| `src/ipp_server.rs`             | Dedicated `std::net::TcpListener` HTTP/1.1 server on port 631 supporting `Expect: 100-continue`, chunked POST streaming, Keep-Alive, connection capping (`MAX_CONCURRENT_IPP_CONNS = 5`), and binary IPP/2.0 URI sanitization.                            |
| `src/ipp.rs` + `src/backend.rs` | Pluggable `ScannerBackend`/`PrinterBackend` traits and a complete RFC 8010/8011 mock fallback (returns `assets/test_scan.jpg` when no USB device is attached).                                                                                            |

---

## 2. Critical Build & Toolchain Invariants (DO NOT BREAK)

1. **Exact Rust target triple:** `riscv32imafc-esp-espidf` (single-precision FPU `f` — **NOT** `riscv32imafdc-esp-espidf`).
2. **Build command:** Always run with `RUSTC_BOOTSTRAP=1` (e.g. `RUSTC_BOOTSTRAP=1 cargo build --release`).
   - _Why:_ `rust-toolchain.toml` pins `stable` (`libc 0.2.189`). Newer `nightly` builds bumped vendored `libc` to `0.2.191`, which changed `newlib::stat` fields (`st_mtime` → `st_mtim`) and broke `std` compilation for `target_os = "espidf"`.
   - To avoid re-downloading ~5 GB of ESP-IDF tools when a local install already exists, pass `ESP_IDF_TOOLS_INSTALL_DIR="custom:<path>/espressif"`.
3. **mDNS component pin (`Cargo.toml`):** `espressif/mdns` is pinned to `~1.8.2`. Do not blindly bump to `1.*` / `1.15.1` without verifying — `1.15.1` failed IDF component manager integrity check (`README.md` missing).
4. **ESP32-P4 silicon revision:** ESP-IDF `v5.5.3` defaults to `ESP32P4_REV_MIN_301` (`!ESP32P4_SELECTS_REV_LESS_V3`), matching production v3.x silicon (e.g. rev v3.2). If targeting an older engineering sample (rev v0.x/v1.x), set `CONFIG_ESP32P4_SELECTS_REV_LESS_V3=y` in `sdkconfig.defaults`.
5. **Custom `partitions.csv` (dual OTA):** Per `esp-idf-sys`, do **NOT** set `CONFIG_PARTITION_TABLE_CUSTOM=y` in `sdkconfig.defaults`; pass `--partition-table partitions.csv` (and `--bootloader <idf_bootloader.bin>` for hardware rollback) to `espflash` via `./flash-usb.sh` or `.cargo/config.toml`. Future wireless updates use `./ota.sh` (`espflash save-image --chip esp32p4 --flash-size 16mb` + `POST /ota`).

---

## 3. Critical Hardware & Protocol Gotchas

1. **Onboard CH334F USB Hub (`sdkconfig.defaults`):**
   - On the Waveshare ESP32-P4-Module-DEV-KIT, 3 of the 4 USB-A ports sit behind an onboard **CH334F** USB 2.0 Hub behind an `FSUSB42UMX` mux controlled by the 3-pin HOST/DEVICE jumper.
   - `CONFIG_USB_HOST_HUBS_SUPPORTED=y` and `CONFIG_USB_HOST_HUB_MULTI_LEVEL=y` are **mandatory**; without them, devices on the USB-A hub ports never enumerate.
2. **ESP-IDF `usb_host_interface_claim` does NOT send `SET_INTERFACE`:**
   - `usb_host_interface_claim()` only allocates host pipes. `ipp_usb::send_set_interface()` MUST explicitly send the EP0 control transfer (`bmRequestType=0x01, bRequest=0x0B, wValue=alt, wIndex=iface`) to switch IPP-USB interfaces into `alt 1`.
   - **Never** send synchronous/blocking control transfers inside `client_event_cb`, because `usb_host_client_handle_events()` on that same thread must run to pump the transfer completion callback. Always run `send_set_interface` on a separate thread (`ipp_usb_init`).
3. **USB Bulk IN transfer sizes:**
   - Must be an exact integer multiple of `wMaxPacketSize` (512 bytes for High-Speed Bulk) to prevent `USB_TRANSFER_STATUS_OVERFLOW`.
4. **Why port 631 MUST NOT use ESP-IDF's `esp_http_server`:**
   - ESP-IDF's `httpd_parse.c` sets `content_len = 0` for incoming `Transfer-Encoding: chunked` requests and does not reply to `Expect: 100-continue`. macOS CUPS/AirPrint uses both. Keep port 631 on `src/ipp_server.rs` (`std::net::TcpListener`).
   - **Never call `TcpStream::try_clone()` on ESP-IDF:** lwIP has no `dup()` and `try_clone()` fails immediately. Use `BufReader::get_mut()` on the single `TcpStream` between sequential reads and writes.
5. **Epson ET-2720 IPP-USB firmware bugs:**
   - Returns malformed URI bytes in `Get-Printer-Attributes` (`http://localhost<garbage>/PRESENTATION/BONJOUR` and `http://localhost /LANGUAGES/IPP?LANG=en`), causing macOS `ipp2ppd` / `ipptool` to fail with `client-error-bad-request`. `ipp_server::sanitize_ipp_response()` repairs `TAG_URI (0x45)` values and rewrites `localhost` → the ESP32-P4 host while updating 2-byte big-endian TLV lengths.
   - Port 80 proxies `/icon.png`, `/PRESENTATION/*`, and `/LANGUAGES/*` so `ipp2ppd` can fetch printer icons and strings referenced by those rewritten URIs.
6. **Concurrency & socket budget:**
   - `CONFIG_LWIP_MAX_SOCKETS=16`. Keep port-80 `max_open_sockets = 5` + port-631 `MAX_CONCURRENT_IPP_CONNS = 5` so total sockets (including listeners + mDNS) stay safely below 16.
   - In `IppUsbBridge`, `connected` MUST remain an `AtomicBool` (never a `Mutex` nested with `pool`) to avoid lock-order deadlocks on USB disconnect (`DEV_GONE`).
   - If an HTTP stream aborts mid-response (e.g. canceled scan), `release_channel(..., was_err = true)` drains residual Bulk IN bytes so the persistent IPP-USB channel stays framed for the next HTTP request.

---

## 4. Verification Commands (from macOS on Same LAN)

```bash
# 1. Verify mDNS advertisement
dns-sd -B _uscan._tcp local.
dns-sd -B _ipp._tcp local.

# 2. Verify USB enumeration & IPP-USB channels
printf 'GET /usb HTTP/1.1\r\nHost: p4-airbridge.local\r\nConnection: close\r\n\r\n' | nc -w 4 p4-airbridge.local 80

# 3. Verify AirScan eSCL capabilities
printf 'GET /eSCL/ScannerCapabilities HTTP/1.1\r\nHost: p4-airbridge.local\r\nConnection: close\r\n\r\n' | nc -w 4 p4-airbridge.local 80

# 4. Verify AirPrint IPP/2.0 (must report [PASS])
/usr/bin/ipptool -tv ipp://p4-airbridge.local:631/ipp/print /usr/share/cups/ipptool/get-printer-attributes.test
```

_(Note: on newer macOS versions, sandboxed/non-Apple CLI tools like `curl` or `python3` run from child processes may get `EHOSTUNREACH` on LAN IPs due to Local Network privacy, while Apple-signed `/usr/bin/nc`, `/usr/bin/ping`, `/usr/bin/dns-sd`, and `/usr/bin/ipptool` work reliably.)_
