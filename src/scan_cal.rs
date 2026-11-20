use std::sync::Mutex;

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

/// Calibrated scan parameters injected into eSCL `POST /eSCL/ScanJobs` XML
/// before forwarding to the Epson ET-2720's native 16-bit CIS sensor pipeline.
#[derive(Debug, Clone, Copy)]
pub struct ScanCalibration {
    pub enabled: bool,
    /// Epson ET-2720 range: -100..=100 (default 0; negative pulls lifted shadow floor down)
    pub brightness: i32,
    /// Epson ET-2720 range: -100..=100 (default 0; positive deepens blacks & punchiness)
    pub contrast: i32,
    /// Epson ET-2720 range: 0..=3 (default 2)
    pub sharpen: i32,
    /// Epson ET-2720 range: 1..=3 (1 = highest JPEG quality, 2 = normal, 3 = small)
    pub compression_factor: i32,
}

impl Default for ScanCalibration {
    fn default() -> Self {
        // Golden sweet spot tuned from N/V/D.jpeg multi-card calibration:
        // - N (-10/+16) left blacks slightly lifted (#302D2E)
        // - V (-18/+24) darkened ADAC yellow slightly amber (#FDB002)
        // - D (-10/+40) nailed ADAC yellow (#FDC500) & deep black (#1A1819) but barely touched 255 on red/cyan
        // => -8/+30 hits true bright yellow, crisp black, and zero highlight clipping!
        Self {
            enabled: true,
            brightness: -8,
            contrast: 30,
            sharpen: 2,
            compression_factor: 1,
        }
    }
}

impl ScanCalibration {
    pub fn clamp(self) -> Self {
        Self {
            enabled: self.enabled,
            brightness: self.brightness.clamp(-100, 100),
            contrast: self.contrast.clamp(-100, 100),
            sharpen: self.sharpen.clamp(0, 3),
            compression_factor: self.compression_factor.clamp(1, 3),
        }
    }

    pub fn preset(name: &str) -> Option<Self> {
        match name {
            "balanced" | "natural" => Some(Self::default()),
            "soft" => Some(Self {
                enabled: true,
                brightness: -10,
                contrast: 16,
                sharpen: 2,
                compression_factor: 1,
            }),
            "vivid" => Some(Self {
                enabled: true,
                brightness: -18,
                contrast: 24,
                sharpen: 2,
                compression_factor: 1,
            }),
            "document" => Some(Self {
                enabled: true,
                brightness: -10,
                contrast: 40,
                sharpen: 3,
                compression_factor: 1,
            }),
            "raw" => Some(Self {
                enabled: false,
                brightness: 0,
                contrast: 0,
                sharpen: 2,
                compression_factor: 1,
            }),
            _ => None,
        }
    }
}

pub struct ScanCalStore {
    current: Mutex<ScanCalibration>,
    nvs: Mutex<Option<EspNvs<NvsDefault>>>,
}

impl ScanCalStore {
    pub fn new(partition: EspDefaultNvsPartition) -> Self {
        let mut cal = ScanCalibration::default();
        let nvs_opt = match EspNvs::new(partition, "airbridge", true) {
            Ok(nvs) => {
                if let Ok(Some(v)) = nvs.get_i32("cal_en") {
                    cal.enabled = v != 0;
                }
                if let Ok(Some(v)) = nvs.get_i32("cal_bri") {
                    cal.brightness = v;
                }
                if let Ok(Some(v)) = nvs.get_i32("cal_con") {
                    cal.contrast = v;
                }
                if let Ok(Some(v)) = nvs.get_i32("cal_shp") {
                    cal.sharpen = v;
                }
                if let Ok(Some(v)) = nvs.get_i32("cal_cmp") {
                    cal.compression_factor = v;
                }
                cal = cal.clamp();
                log::info!("[ScanCal] Loaded from NVS: {:?}", cal);
                Some(nvs)
            }
            Err(e) => {
                log::warn!("[ScanCal] NVS open failed (using RAM defaults): {:?}", e);
                None
            }
        };

        Self {
            current: Mutex::new(cal),
            nvs: Mutex::new(nvs_opt),
        }
    }

    pub fn get(&self) -> ScanCalibration {
        *self.current.lock().unwrap()
    }

    pub fn set(&self, cal: ScanCalibration) -> ScanCalibration {
        let cal = cal.clamp();
        *self.current.lock().unwrap() = cal;
        if let Ok(mut guard) = self.nvs.lock() {
            if let Some(nvs) = guard.as_mut() {
                let _ = nvs.set_i32("cal_en", if cal.enabled { 1 } else { 0 });
                let _ = nvs.set_i32("cal_bri", cal.brightness);
                let _ = nvs.set_i32("cal_con", cal.contrast);
                let _ = nvs.set_i32("cal_shp", cal.sharpen);
                let _ = nvs.set_i32("cal_cmp", cal.compression_factor);
            }
        }
        log::info!("[ScanCal] Updated & saved: {:?}", cal);
        cal
    }

    /// Inject calibrated Epson hardware eSCL elements (`Brightness`, `Contrast`, `Sharpen`,
    /// `CompressionFactor`) into `POST /eSCL/ScanJobs` XML whenever the client didn't
    /// already explicitly supply them.
    pub fn rewrite_scan_settings(&self, raw_xml: &[u8]) -> Vec<u8> {
        let cal = self.get();
        let xml = String::from_utf8_lossy(raw_xml);

        // Detect closing ScanSettings tag and its namespace prefix (e.g. `scan:` or empty).
        let Some((close_pos, prefix)) = find_scan_settings_close(&xml) else {
            return raw_xml.to_vec();
        };

        let mut injected = String::new();

        // Always prefer highest JPEG quality unless the client explicitly requested a factor.
        if !has_xml_element(&xml, "CompressionFactor") {
            injected.push_str(&format!(
                "<{p}CompressionFactor>{}</{p}CompressionFactor>",
                cal.compression_factor,
                p = prefix
            ));
        }

        if cal.enabled {
            if !has_xml_element(&xml, "Brightness") && cal.brightness != 0 {
                injected.push_str(&format!(
                    "<{p}Brightness>{}</{p}Brightness>",
                    cal.brightness,
                    p = prefix
                ));
            }
            if !has_xml_element(&xml, "Contrast") && cal.contrast != 0 {
                injected.push_str(&format!(
                    "<{p}Contrast>{}</{p}Contrast>",
                    cal.contrast,
                    p = prefix
                ));
            }
            if !has_xml_element(&xml, "Sharpen") {
                injected.push_str(&format!(
                    "<{p}Sharpen>{}</{p}Sharpen>",
                    cal.sharpen,
                    p = prefix
                ));
            }
        }

        if injected.is_empty() {
            return raw_xml.to_vec();
        }

        log::info!("[ScanCal] Injecting into ScanSettings: {}", injected);

        let mut out = String::with_capacity(xml.len() + injected.len());
        out.push_str(&xml[..close_pos]);
        out.push_str(&injected);
        out.push_str(&xml[close_pos..]);
        out.into_bytes()
    }
}

/// Find `</scan:ScanSettings>` or `</ScanSettings>` (or any prefix like `</ns0:ScanSettings>`).
fn find_scan_settings_close(xml: &str) -> Option<(usize, &str)> {
    let idx = xml.rfind("ScanSettings>")?;
    let before = &xml[..idx];
    let slash_pos = before.rfind("</")?;
    let prefix = &xml[slash_pos + 2..idx];
    Some((slash_pos, prefix))
}

fn has_xml_element(xml: &str, name: &str) -> bool {
    xml.contains(&format!("<{}>", name))
        || xml.contains(&format!(":{}>", name))
        || xml.contains(&format!("<{} ", name))
        || xml.contains(&format!(":{} ", name))
}
