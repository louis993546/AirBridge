mod backend;
mod config;
mod escl;
mod eth;
mod ipp;
mod ipp_server;
mod ipp_usb;
mod mdns;
mod scan_cal;
mod usb_host;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::http::server::{Configuration as HttpConfig, EspHttpServer};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::info;

use crate::backend::{MockEpsonBridge, PrinterBackend, ScannerBackend};
use crate::escl::EsclState;
use crate::eth::P4Ethernet;
use crate::ipp_usb::IppUsbBridge;
use crate::scan_cal::{ScanCalStore, ScanCalibration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Necessary ESP-IDF runtime glue & USB-Serial/JTAG logger
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("========================================");
    info!("  ESP32-P4 AirBridge (AirScan + AirPrint)");
    info!("  Board: Waveshare ESP32-P4-Module-DEV-KIT");
    info!("========================================");

    // 2. Core ESP-IDF services + persistent scan calibration store in NVS
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;
    let scan_cal = Arc::new(ScanCalStore::new(nvs));

    // 3. Bring up 100M Ethernet (non-blocking if RJ45 isn't plugged in yet)
    let ethernet = P4Ethernet::start(&sys_loop)?;

    // 3b. Bring up High-Speed USB 2.0 Host + IPP-USB bridge (with CH334F Hub support)
    let ipp_usb = Arc::new(IppUsbBridge::new());
    let usb_state = usb_host::start_usb_host(ipp_usb.clone())?;

    // 4. Fallback mock backend when no USB printer is connected
    let mock_bridge = Arc::new(MockEpsonBridge::new());
    let scanner_backend: Arc<dyn ScannerBackend> = mock_bridge.clone();
    let printer_backend: Arc<dyn PrinterBackend> = mock_bridge.clone();

    // 5. Advertise AirScan (_uscan._tcp:80) and AirPrint (_ipp._tcp:631 + _universal) via mDNS
    let _mdns = mdns::start_mdns(scanner_backend.as_ref(), printer_backend.as_ref())?;

    // 6. Start HTTP Server #1 on port 80: Status + Calibration UI + AirScan (eSCL) + Epson Assets
    let mut http_server = EspHttpServer::new(&HttpConfig {
        http_port: mdns::AIRSCAN_PORT,
        ctrl_port: 32768,
        stack_size: 20480,
        max_open_sockets: 5,
        max_uri_handlers: 22,
        uri_match_wildcard: true,
        ..Default::default()
    })?;

    let boot_instant = std::time::Instant::now();

    // Status & Scan Calibration dashboard at http://p4-airbridge.local/
    {
        let net_state = ethernet.state.clone();
        let usb = usb_state.clone();
        let ipp_usb_ref = ipp_usb.clone();
        let cal_ref = scan_cal.clone();
        http_server.fn_handler("/", Method::Get, move |req| {
            let ip_str = net_state
                .lock()
                .unwrap()
                .ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "Pending DHCP".to_string());
            let usb_text = usb.lock().unwrap().summary_text();
            let cal = cal_ref.get();
            let uptime_s = boot_instant.elapsed().as_secs();
            let uptime_str = format!(
                "{}d {}h {}m {}s",
                uptime_s / 86400,
                (uptime_s % 86400) / 3600,
                (uptime_s % 3600) / 60,
                uptime_s % 60
            );
            let free_internal_kb = unsafe {
                esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_INTERNAL)
            } / 1024;
            let free_psram_kb = unsafe {
                esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_SPIRAM)
            } / 1024;
            let mode_str = if ipp_usb_ref.is_connected() {
                format!("LIVE IPP-USB BRIDGE ({})", ipp_usb_ref.product_name())
            } else {
                "Mock Backend (plug in USB printer to switch to live IPP-USB)".to_string()
            };
            let checked = if cal.enabled { "checked" } else { "" };
            let cmp1 = if cal.compression_factor == 1 { "selected" } else { "" };
            let cmp2 = if cal.compression_factor == 2 { "selected" } else { "" };
            let cmp3 = if cal.compression_factor == 3 { "selected" } else { "" };

            let html = format!(
                r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>ESP32-P4 AirBridge</title>
<style>
body{{font-family:-apple-system,sans-serif;max-width:760px;margin:1.5rem auto;padding:0 1rem;line-height:1.5;color:#0f172a;}}
code,pre{{background:#f1f5f9;padding:2px 6px;border-radius:4px;}} pre{{padding:12px;overflow-x:auto;font-size:12px;}}
.card{{border:1px solid #cbd5e1;border-radius:10px;padding:16px;margin:16px 0;background:#f8fafc;}}
.row{{display:flex;align-items:center;gap:12px;margin:10px 0;}}
.row label{{width:140px;font-weight:600;}}
.row input[type=range]{{flex:1;}}
.val{{width:44px;text-align:right;font-variant-numeric:tabular-nums;font-weight:600;}}
.btns{{display:flex;flex-wrap:wrap;gap:8px;margin:12px 0;}}
.btn{{display:inline-block;padding:7px 12px;border-radius:6px;border:1px solid #94a3b8;background:#fff;color:#0f172a;text-decoration:none;cursor:pointer;font-size:14px;}}
.btn.primary{{background:#0284c7;color:#fff;border-color:#0284c7;}}
.btn.danger{{color:#b91c1c;border-color:#fca5a5;}}
</style></head>
<body>
  <h1>ESP32-P4 AirBridge</h1>
  <p>
    <strong>Mode:</strong> {mode}<br>
    <strong>IP:</strong> <code>{ip}</code> (<code>p4-airbridge.local</code>)<br>
    <strong>Uptime:</strong> {uptime} &nbsp;|&nbsp; <strong>Free RAM:</strong> {ram_kb} KB internal, {psram_kb} KB PSRAM
  </p>

  <div class="card">
    <h2 style="margin-top:0">Scan Calibration (Epson Hardware eSCL)</h2>
    <p style="margin-top:0;font-size:13px;color:#475569">Injected into <code>POST /eSCL/ScanJobs</code> inside the Epson's 16-bit sensor pipeline (saved to NVS across reboots). Manual sliders inside macOS Image Capture override these.</p>
    <div class="btns">
      <a class="btn" href="/cal/set?preset=balanced">Balanced (Default: -8 / +30)</a>
      <a class="btn" href="/cal/set?preset=soft">Soft (-10 / +16)</a>
      <a class="btn" href="/cal/set?preset=vivid">Vivid (-18 / +24)</a>
      <a class="btn" href="/cal/set?preset=document">Document Crisp (-10 / +40)</a>
      <a class="btn" href="/cal/set?preset=raw">Raw Epson (Off)</a>
    </div>
    <form action="/cal/set" method="get">
      <div class="row">
        <label for="en">Calibration Active</label>
        <input type="checkbox" id="en" name="en" value="1" {checked}>
      </div>
      <div class="row">
        <label for="bri">Brightness (-100..100)</label>
        <input type="range" id="bri" name="bri" min="-100" max="100" value="{bri}" oninput="bv.textContent=this.value">
        <span class="val" id="bv">{bri}</span>
      </div>
      <div class="row">
        <label for="con">Contrast (-100..100)</label>
        <input type="range" id="con" name="con" min="-100" max="100" value="{con}" oninput="cv.textContent=this.value">
        <span class="val" id="cv">{con}</span>
      </div>
      <div class="row">
        <label for="shp">Sharpen (0..3)</label>
        <input type="range" id="shp" name="shp" min="0" max="3" value="{shp}" oninput="sv.textContent=this.value">
        <span class="val" id="sv">{shp}</span>
      </div>
      <div class="row">
        <label for="cmp">JPEG Quality</label>
        <select id="cmp" name="cmp">
          <option value="1" {cmp1}>1 — Highest Quality (Best)</option>
          <option value="2" {cmp2}>2 — Normal (Epson Default)</option>
          <option value="3" {cmp3}>3 — Smallest File</option>
        </select>
      </div>
      <button class="btn primary" type="submit">Save Calibration</button>
    </form>
  </div>

  <h2>Connected USB Devices</h2>
  <pre>{usb_text}</pre>
  <h2>Endpoints &amp; Maintenance</h2>
  <ul>
    <li>USB Inspector (text): <a href="/usb"><code>/usb</code></a></li>
    <li>AirScan Capabilities: <a href="/eSCL/ScannerCapabilities"><code>/eSCL/ScannerCapabilities</code></a></li>
    <li>AirScan Status: <a href="/eSCL/ScannerStatus"><code>/eSCL/ScannerStatus</code></a></li>
    <li>AirPrint IPP: <code>ipp://{ip}:631/ipp/print</code></li>
  </ul>
  <p><a class="btn danger" href="/reboot" onclick="return confirm('Reboot ESP32-P4 AirBridge?')">Reboot ESP32-P4</a></p>
</body>
</html>"#,
                mode = mode_str,
                ip = ip_str,
                uptime = uptime_str,
                ram_kb = free_internal_kb,
                psram_kb = free_psram_kb,
                checked = checked,
                bri = cal.brightness,
                con = cal.contrast,
                shp = cal.sharpen,
                cmp1 = cmp1,
                cmp2 = cmp2,
                cmp3 = cmp3,
                usb_text = usb_text,
            );
            let mut resp = req.into_response(
                200,
                Some("OK"),
                &[("Content-Type", "text/html; charset=utf-8")],
            )?;
            resp.write_all(html.as_bytes())?;
            Ok::<(), esp_idf_svc::io::EspIOError>(())
        })?;
    }

    // Remote reboot endpoint for headless operation behind the printer
    http_server.fn_handler("/reboot", Method::Get, move |req| {
        let mut resp = req.into_response(
            200,
            Some("OK"),
            &[("Content-Type", "text/html; charset=utf-8")],
        )?;
        resp.write_all(
            b"<html><head><meta http-equiv=\"refresh\" content=\"8;url=/\"></head><body>Rebooting ESP32-P4 AirBridge... refreshing in 8s.</body></html>",
        )?;
        let _ = resp.flush();
        thread::spawn(|| {
            thread::sleep(Duration::from_millis(500));
            unsafe { esp_idf_svc::sys::esp_restart() };
        });
        Ok::<(), esp_idf_svc::io::EspIOError>(())
    })?;

    // Update calibration settings via query string: /cal/set?preset=natural OR /cal/set?en=1&bri=-18&con=24&shp=2&cmp=1
    {
        let cal_ref = scan_cal.clone();
        http_server.fn_handler("/cal/set", Method::Get, move |req| {
            let uri = req.uri().to_string();
            let query = uri.split_once('?').map(|(_, q)| q).unwrap_or("");
            let mut next = cal_ref.get();
            let mut applied_preset = false;
            let mut saw_form_fields = false;
            let mut en_flag = false;

            for pair in query.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    match k {
                        "preset" => {
                            if let Some(p) = ScanCalibration::preset(v) {
                                next = p;
                                applied_preset = true;
                            }
                        }
                        "en" => {
                            en_flag = v == "1" || v == "true" || v == "on";
                        }
                        "bri" => {
                            saw_form_fields = true;
                            if let Ok(n) = v.parse::<i32>() {
                                next.brightness = n;
                            }
                        }
                        "con" => {
                            saw_form_fields = true;
                            if let Ok(n) = v.parse::<i32>() {
                                next.contrast = n;
                            }
                        }
                        "shp" => {
                            saw_form_fields = true;
                            if let Ok(n) = v.parse::<i32>() {
                                next.sharpen = n;
                            }
                        }
                        "cmp" => {
                            saw_form_fields = true;
                            if let Ok(n) = v.parse::<i32>() {
                                next.compression_factor = n;
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !applied_preset && saw_form_fields {
                next.enabled = en_flag;
            }

            cal_ref.set(next);

            let mut resp = req.into_response(
                303,
                Some("See Other"),
                &[("Location", "/"), ("Content-Length", "0")],
            )?;
            resp.flush()?;
            Ok::<(), esp_idf_svc::io::EspIOError>(())
        })?;
    }

    // Plain-text USB device descriptor dump at /usb
    {
        let usb = usb_state.clone();
        http_server.fn_handler("/usb", Method::Get, move |req| {
            let text = usb.lock().unwrap().summary_text();
            let mut resp = req.into_response(
                200,
                Some("OK"),
                &[("Content-Type", "text/plain; charset=utf-8")],
            )?;
            resp.write_all(text.as_bytes())?;
            Ok::<(), esp_idf_svc::io::EspIOError>(())
        })?;
    }

    let escl_state = Arc::new(EsclState::new(scanner_backend, ipp_usb.clone(), scan_cal));
    escl::register_routes(&mut http_server, escl_state)?;
    ipp::register_routes(
        &mut http_server,
        printer_backend.clone(),
        ipp_usb.clone(),
        false,
    )?;

    // Proxy Epson's icon, localization strings, and web presentation pages over port 80
    for asset_path in ["/icon.png", "/PRESENTATION/*", "/LANGUAGES/*"] {
        let usb = ipp_usb.clone();
        http_server.fn_handler(asset_path, Method::Get, move |req| {
            if !usb.is_connected() {
                let mut resp = req.into_response(404, Some("Not Found"), &[])?;
                resp.flush()?;
                return Ok::<(), esp_idf_svc::io::EspIOError>(());
            }
            let uri = req.uri().to_string();
            let mut req_opt = Some(req);
            let res = usb.exchange_streaming(
                "GET",
                &uri,
                None,
                &[],
                |head| {
                    let r = req_opt.take().ok_or("req consumed")?;
                    let ct = head
                        .header("Content-Type")
                        .unwrap_or("application/octet-stream")
                        .to_string();
                    r.into_response(
                        head.status_code,
                        Some(head.status_text.as_str()),
                        &[("Content-Type", ct.as_str())],
                    )
                    .map_err(|e| format!("into_response: {:?}", e))
                },
                |resp, chunk| {
                    resp.write_all(chunk)
                        .map_err(|e| format!("write: {:?}", e))
                },
            );
            if let Ok(mut resp) = res {
                let _ = resp.flush();
            } else if let Some(r) = req_opt {
                let mut resp = r.into_response(502, Some("Bad Gateway"), &[])?;
                resp.flush()?;
            }
            Ok(())
        })?;
    }

    info!(
        "HTTP Server #1 listening on port {} (Web UI + Scan Calibration + AirScan eSCL)",
        mdns::AIRSCAN_PORT
    );

    // 7. Dedicated pure-Rust HTTP/1.1 AirPrint IPP server on port 631
    ipp_server::start_ipp_tcp_server(mdns::AIRPRINT_PORT, printer_backend, ipp_usb)?;

    info!("Ready! AirScan & AirPrint bridge active.");

    loop {
        thread::sleep(Duration::from_secs(10));
    }
}
