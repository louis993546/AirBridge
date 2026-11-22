//! Compile-time device identity & mDNS configuration loaded from optional gitignored `cfg.toml`
//! (via `build.rs`), falling back to safe generic defaults for public builds.

pub const MDNS_HOSTNAME: &str = match option_env!("AIRBRIDGE_MDNS_HOSTNAME") {
    Some(v) => v,
    None => "p4-airbridge",
};

pub const PRINTER_NAME: &str = match option_env!("AIRBRIDGE_PRINTER_NAME") {
    Some(v) => v,
    None => "P4 AirBridge Printer",
};

pub const PRINTER_MODEL: &str = match option_env!("AIRBRIDGE_PRINTER_MODEL") {
    Some(v) => v,
    None => "Generic IPP-USB MFP",
};

pub const PRINTER_PRODUCT: &str = match option_env!("AIRBRIDGE_PRINTER_PRODUCT") {
    Some(v) => v,
    None => "(Generic IPP-USB MFP)",
};

pub const PRINTER_SERIAL: &str = match option_env!("AIRBRIDGE_PRINTER_SERIAL") {
    Some(v) => v,
    None => "P4-AIRBRIDGE-001",
};

pub const PRINTER_UUID: &str = match option_env!("AIRBRIDGE_PRINTER_UUID") {
    Some(v) => v,
    None => "00000000-0000-4000-8000-000000000001",
};

pub const PRINTER_PDL: &str = match option_env!("AIRBRIDGE_PRINTER_PDL") {
    Some(v) => v,
    None => "application/octet-stream,image/pwg-raster,image/urf,image/jpeg",
};

pub const PRINTER_URF: &str = match option_env!("AIRBRIDGE_PRINTER_URF") {
    Some(v) => v,
    None => "W8,SRGB24,RS300-600,DM1,PQ4",
};

pub const SCANNER_PDL: &str = match option_env!("AIRBRIDGE_SCANNER_PDL") {
    Some(v) => v,
    None => "application/pdf,image/jpeg",
};

pub const SCANNER_CS: &str = match option_env!("AIRBRIDGE_SCANNER_CS") {
    Some(v) => v,
    None => "color,grayscale,binary",
};

pub const WIFI_SSID: &str = match option_env!("AIRBRIDGE_WIFI_SSID") {
    Some(v) => v,
    None => "",
};

pub const WIFI_PASS: &str = match option_env!("AIRBRIDGE_WIFI_PASS") {
    Some(v) => v,
    None => "",
};

pub const WIFI_COUNTRY: &str = match option_env!("AIRBRIDGE_WIFI_COUNTRY") {
    Some(v) => v,
    None => "EU",
};

/// Optional shared secret required on `POST /ota` (header `X-OTA-Token` or `?token=...`).
/// Leave empty in `cfg.toml` to allow unauthenticated LAN OTA updates.
pub const OTA_TOKEN: &str = match option_env!("AIRBRIDGE_OTA_TOKEN") {
    Some(v) => v,
    None => "",
};
