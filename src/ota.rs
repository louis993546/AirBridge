use std::ffi::CStr;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use esp_idf_svc::sys;

use crate::config;
use crate::eth::NetworkState;
use crate::wifi::WifiState;

/// Minimum plausible ESP-IDF app binary size (64 KB) and max size matching 4 MB OTA slot.
const MIN_OTA_BYTES: usize = 64 * 1024;
const MAX_OTA_BYTES: usize = 4 * 1024 * 1024;
/// ESP-IDF firmware binary header magic byte (`ESP_IMAGE_HEADER_MAGIC`).
const ESP_IMAGE_MAGIC: u8 = 0xE9;

/// Return a human-readable summary of the currently running partition & app build info.
pub fn running_partition_summary() -> String {
    unsafe {
        let part = sys::esp_ota_get_running_partition();
        let label = if !part.is_null() {
            CStr::from_ptr((*part).label.as_ptr())
                .to_string_lossy()
                .into_owned()
        } else {
            "unknown".to_string()
        };

        let desc = sys::esp_app_get_description();
        let (date, time) = if !desc.is_null() {
            (
                CStr::from_ptr((*desc).date.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
                CStr::from_ptr((*desc).time.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            )
        } else {
            ("?".to_string(), "?".to_string())
        };

        format!("slot={} (built {} {})", label, date, time)
    }
}

/// Confirm the running OTA firmware as valid once network connectivity is up.
/// If a bad OTA update panics or fails to connect to Wi-Fi/Ethernet before this runs,
/// the ESP-IDF bootloader (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`) automatically
/// rolls back to the previous working slot on the next reboot.
pub fn spawn_rollback_confirmation(
    eth_state: Arc<Mutex<NetworkState>>,
    wifi_state: Arc<Mutex<WifiState>>,
) {
    let _ = thread::Builder::new()
        .name("ota_confirm".into())
        .stack_size(4096)
        .spawn(move || {
            // Wait until either Ethernet or Wi-Fi gets an IP (up to 60s)
            for _ in 0..120 {
                let eth_ok = eth_state.lock().map(|s| s.ip.is_some()).unwrap_or(false);
                let wifi_ok = wifi_state.lock().map(|s| s.ip.is_some()).unwrap_or(false);
                if eth_ok || wifi_ok {
                    // Let HTTP servers settle for a moment before confirming
                    thread::sleep(Duration::from_secs(3));
                    let err = unsafe { sys::esp_ota_mark_app_valid_cancel_rollback() };
                    log::info!(
                        "[OTA] Network healthy; marked firmware valid ({}, err={})",
                        running_partition_summary(),
                        err
                    );
                    return;
                }
                thread::sleep(Duration::from_millis(500));
            }
            log::warn!(
                "[OTA] No network IP after 60s — leaving OTA rollback armed if pending verify"
            );
        });
}

/// Register `POST /ota` on the port-80 HTTP server for wireless firmware updates.
pub fn register_routes(
    server: &mut EspHttpServer<'static>,
) -> Result<(), Box<dyn std::error::Error>> {
    server.fn_handler("/ota", Method::Post, |mut req| {
        // 1. Optional token authentication (set OTA_TOKEN in cfg.toml)
        if !config::OTA_TOKEN.is_empty() {
            let hdr_tok = req.header("X-OTA-Token").unwrap_or("");
            let query_tok = extract_query_param(req.uri(), "token").unwrap_or("");
            if hdr_tok != config::OTA_TOKEN && query_tok != config::OTA_TOKEN {
                log::warn!("[OTA] Rejected unauthorized upload attempt");
                let mut resp = req.into_response(
                    401,
                    Some("Unauthorized"),
                    &[("Content-Type", "text/plain")],
                )?;
                resp.write_all(b"Unauthorized: missing or invalid OTA_TOKEN\n")?;
                return Ok::<(), esp_idf_svc::io::EspIOError>(());
            }
        }

        // 2. Validate Content-Length before touching flash
        let content_len = req
            .header("Content-Length")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if !(MIN_OTA_BYTES..=MAX_OTA_BYTES).contains(&content_len) {
            let msg = format!(
                "Invalid Content-Length: {} bytes (expected {}..={} bytes of ESP-IDF .bin)\n",
                content_len, MIN_OTA_BYTES, MAX_OTA_BYTES
            );
            log::warn!("[OTA] {}", msg.trim());
            let mut resp = req.into_response(
                400,
                Some("Bad Request"),
                &[("Content-Type", "text/plain")],
            )?;
            resp.write_all(msg.as_bytes())?;
            return Ok(());
        }

        // 3. Locate inactive OTA partition
        let update_part = unsafe { sys::esp_ota_get_next_update_partition(ptr::null()) };
        if update_part.is_null() {
            let msg = "No OTA update partition found! Flash once over USB with `--partition-table partitions.csv` first.\n";
            log::error!("[OTA] {}", msg.trim());
            let mut resp = req.into_response(
                500,
                Some("Internal Server Error"),
                &[("Content-Type", "text/plain")],
            )?;
            resp.write_all(msg.as_bytes())?;
            return Ok(());
        }

        let target_label = unsafe {
            CStr::from_ptr((*update_part).label.as_ptr())
                .to_string_lossy()
                .into_owned()
        };

        // 4. Read first chunk and verify ESP-IDF image header magic (0xE9) before erasing flash
        let mut buf = [0u8; 4096];
        let first_n = match req.read(&mut buf) {
            Ok(n) if n > 0 => n,
            Ok(_) => {
                let mut resp = req.into_response(
                    400,
                    Some("Bad Request"),
                    &[("Content-Type", "text/plain")],
                )?;
                resp.write_all(b"Empty upload body\n")?;
                return Ok(());
            }
            Err(e) => {
                log::error!("[OTA] Read failed: {:?}", e);
                let mut resp = req.into_response(
                    400,
                    Some("Bad Request"),
                    &[("Content-Type", "text/plain")],
                )?;
                resp.write_all(b"Failed to read upload body\n")?;
                return Ok(());
            }
        };

        if buf[0] != ESP_IMAGE_MAGIC {
            let msg = format!(
                "Invalid firmware image (first byte 0x{:02X}, expected 0xE9). Did you upload an ELF instead of `espflash save-image` .bin?\n",
                buf[0]
            );
            log::warn!("[OTA] {}", msg.trim());
            let mut resp = req.into_response(
                400,
                Some("Bad Request"),
                &[("Content-Type", "text/plain")],
            )?;
            resp.write_all(msg.as_bytes())?;
            return Ok(());
        }

        // 5. Begin sequential OTA write (erases sectors on the fly so Wi-Fi stays responsive)
        log::info!(
            "[OTA] Starting wireless update -> partition '{}' ({} bytes)...",
            target_label,
            content_len
        );
        let mut ota_handle: sys::esp_ota_handle_t = 0;
        let begin_err = unsafe {
            sys::esp_ota_begin(
                update_part,
                sys::OTA_WITH_SEQUENTIAL_WRITES as usize,
                &mut ota_handle,
            )
        };
        if begin_err != sys::ESP_OK {
            let msg = format!("esp_ota_begin failed: err={}\n", begin_err);
            log::error!("[OTA] {}", msg.trim());
            let mut resp = req.into_response(
                500,
                Some("Internal Server Error"),
                &[("Content-Type", "text/plain")],
            )?;
            resp.write_all(msg.as_bytes())?;
            return Ok(());
        }

        // Write first chunk
        let mut written: usize = 0;
        let mut next_log_at: usize = 256 * 1024;
        let write_first = unsafe {
            sys::esp_ota_write(ota_handle, buf.as_ptr() as *const _, first_n)
        };
        if write_first != sys::ESP_OK {
            unsafe { sys::esp_ota_abort(ota_handle) };
            let msg = format!("esp_ota_write failed: err={}\n", write_first);
            log::error!("[OTA] {}", msg.trim());
            let mut resp = req.into_response(
                500,
                Some("Internal Server Error"),
                &[("Content-Type", "text/plain")],
            )?;
            resp.write_all(msg.as_bytes())?;
            return Ok(());
        }
        written += first_n;

        // Stream remaining chunks directly into flash
        while written < content_len {
            let to_read = buf.len().min(content_len - written);
            match req.read(&mut buf[..to_read]) {
                Ok(0) => break,
                Ok(n) => {
                    let werr = unsafe {
                        sys::esp_ota_write(ota_handle, buf.as_ptr() as *const _, n)
                    };
                    if werr != sys::ESP_OK {
                        unsafe { sys::esp_ota_abort(ota_handle) };
                        let msg = format!(
                            "esp_ota_write failed at {} bytes: err={}\n",
                            written, werr
                        );
                        log::error!("[OTA] {}", msg.trim());
                        let mut resp = req.into_response(
                            500,
                            Some("Internal Server Error"),
                            &[("Content-Type", "text/plain")],
                        )?;
                        resp.write_all(msg.as_bytes())?;
                        return Ok(());
                    }
                    written += n;
                    if written >= next_log_at {
                        log::info!("[OTA] Progress: {} / {} bytes", written, content_len);
                        next_log_at += 256 * 1024;
                    }
                }
                Err(e) => {
                    unsafe { sys::esp_ota_abort(ota_handle) };
                    let msg = format!("HTTP read error at {} bytes: {:?}\n", written, e);
                    log::error!("[OTA] {}", msg.trim());
                    let mut resp = req.into_response(
                        500,
                        Some("Internal Server Error"),
                        &[("Content-Type", "text/plain")],
                    )?;
                    resp.write_all(msg.as_bytes())?;
                    return Ok(());
                }
            }
        }

        if written != content_len {
            unsafe { sys::esp_ota_abort(ota_handle) };
            let msg = format!(
                "Incomplete OTA upload: received {} of {} bytes\n",
                written, content_len
            );
            log::error!("[OTA] {}", msg.trim());
            let mut resp = req.into_response(
                400,
                Some("Bad Request"),
                &[("Content-Type", "text/plain")],
            )?;
            resp.write_all(msg.as_bytes())?;
            return Ok(());
        }

        // 6. Validate image checksum & switch boot partition
        let end_err = unsafe { sys::esp_ota_end(ota_handle) };
        if end_err != sys::ESP_OK {
            let msg = format!("esp_ota_end validation failed: err={}\n", end_err);
            log::error!("[OTA] {}", msg.trim());
            let mut resp = req.into_response(
                400,
                Some("Bad Request"),
                &[("Content-Type", "text/plain")],
            )?;
            resp.write_all(msg.as_bytes())?;
            return Ok(());
        }

        let boot_err = unsafe { sys::esp_ota_set_boot_partition(update_part) };
        if boot_err != sys::ESP_OK {
            let msg = format!("esp_ota_set_boot_partition failed: err={}\n", boot_err);
            log::error!("[OTA] {}", msg.trim());
            let mut resp = req.into_response(
                500,
                Some("Internal Server Error"),
                &[("Content-Type", "text/plain")],
            )?;
            resp.write_all(msg.as_bytes())?;
            return Ok(());
        }

        let ok_msg = format!(
            "OTA update succeeded ({} bytes written to '{}'). Rebooting...\n",
            written, target_label
        );
        log::info!("[OTA] {}", ok_msg.trim());
        let mut resp = req.into_response(
            200,
            Some("OK"),
            &[("Content-Type", "text/plain")],
        )?;
        resp.write_all(ok_msg.as_bytes())?;
        let _ = resp.flush();

        // Reboot cleanly into the newly flashed OTA slot
        thread::spawn(|| {
            thread::sleep(Duration::from_millis(600));
            unsafe { sys::esp_restart() };
        });

        Ok(())
    })?;

    Ok(())
}

fn extract_query_param<'a>(uri: &'a str, key: &str) -> Option<&'a str> {
    let (_, query) = uri.split_once('?')?;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v);
            }
        }
    }
    None
}
