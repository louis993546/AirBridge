use std::ffi::CString;

use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::sys::{self, EspError};

use crate::backend::{PrinterBackend, ScannerBackend};
use crate::config;

pub const HOSTNAME: &str = config::MDNS_HOSTNAME;
pub const AIRSCAN_PORT: u16 = 80;
pub const AIRPRINT_PORT: u16 = 631;

/// Start mDNS (Bonjour) and advertise both AirScan (_uscan._tcp) and AirPrint (_ipp._tcp + _universal).
pub fn start_mdns(
    scanner: &dyn ScannerBackend,
    printer: &dyn PrinterBackend,
) -> Result<EspMdns, EspError> {
    let mut mdns = EspMdns::take()?;
    mdns.set_hostname(HOSTNAME)?;
    mdns.set_instance_name(printer.printer_name())?;

    let admin_url = format!("http://{}.local/", HOSTNAME);

    // 1. AirScan / eSCL discovery (_uscan._tcp)
    let uscan_txt = [
        ("txtvers", "1"),
        ("vers", "2.63"),
        ("rs", "eSCL"),
        ("ty", scanner.make_and_model()),
        ("note", "ESP32-P4 USB Bridge"),
        ("adminurl", admin_url.as_str()),
        ("pdl", config::SCANNER_PDL),
        ("cs", config::SCANNER_CS),
        ("is", "platen"),
        ("duplex", "F"),
        ("UUID", scanner.uuid()),
    ];
    mdns.add_service(
        Some(printer.printer_name()),
        "_uscan",
        "_tcp",
        AIRSCAN_PORT,
        &uscan_txt,
    )?;

    // 2. AirPrint / IPP discovery (_ipp._tcp)
    let ipp_txt = [
        ("txtvers", "1"),
        ("qtotal", "1"),
        ("rp", "ipp/print"),
        ("ty", printer.make_and_model()),
        ("adminurl", admin_url.as_str()),
        ("note", "ESP32-P4 USB Bridge"),
        ("priority", "30"),
        ("product", config::PRINTER_PRODUCT),
        ("pdl", config::PRINTER_PDL),
        ("URF", config::PRINTER_URF),
        ("Color", "T"),
        ("Duplex", "F"),
        ("Scan", "T"),
        ("Copies", "T"),
        ("PaperMax", "legal-A4"),
        ("kind", "document,envelope,photo"),
        ("UUID", printer.uuid()),
    ];
    mdns.add_service(
        Some(printer.printer_name()),
        "_ipp",
        "_tcp",
        AIRPRINT_PORT,
        &ipp_txt,
    )?;

    // Register the _universal._sub._ipp._tcp subtype required by Apple AirPrint (iOS/iPadOS/macOS).
    unsafe {
        let instance = CString::new(printer.printer_name()).unwrap();
        let service = CString::new("_ipp").unwrap();
        let proto = CString::new("_tcp").unwrap();
        let subtype = CString::new("_universal").unwrap();
        let err = sys::mdns_service_subtype_add_for_host(
            instance.as_ptr(),
            service.as_ptr(),
            proto.as_ptr(),
            core::ptr::null(),
            subtype.as_ptr(),
        );
        if err != sys::ESP_OK {
            log::warn!(
                "mdns_service_subtype_add_for_host(_universal) returned {:?}",
                err
            );
        } else {
            log::info!("mDNS subtype _universal._sub._ipp._tcp registered");
        }
    }

    // 3. HTTP web status discovery (_http._tcp)
    mdns.add_service(
        Some(printer.printer_name()),
        "_http",
        "_tcp",
        AIRSCAN_PORT,
        &[("path", "/")],
    )?;

    log::info!(
        "mDNS active: {}.local | AirScan (_uscan._tcp:{}) | AirPrint (_ipp._tcp:{})",
        HOSTNAME,
        AIRSCAN_PORT,
        AIRPRINT_PORT
    );

    Ok(mdns)
}
