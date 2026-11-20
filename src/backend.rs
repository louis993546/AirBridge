use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Built-in JPEG test page returned by the mock scanner until a real USB scanner is connected.
const TEST_SCAN_JPEG: &[u8] = include_bytes!("../assets/test_scan.jpg");

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub x_resolution: u32,
    pub y_resolution: u32,
    pub color_mode: String,
    pub document_format: String,
    pub input_source: String,
}

impl Default for ScanRequest {
    fn default() -> Self {
        Self {
            x_resolution: 300,
            y_resolution: 300,
            color_mode: "RGB24".to_string(),
            document_format: "image/jpeg".to_string(),
            input_source: "Platen".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanDocument {
    pub mime_type: &'static str,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PrintJobInfo {
    pub id: u32,
    pub name: String,
    pub format: String,
    pub bytes_received: usize,
    pub state: PrintJobState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PrintJobState {
    Pending = 3,
    Processing = 5,
    Canceled = 7,
    Completed = 9,
}

/// Pluggable backend for scanning (mock today; USB Epson EcoTank tomorrow).
pub trait ScannerBackend: Send + Sync {
    fn make_and_model(&self) -> &str;
    fn serial_number(&self) -> &str;
    fn uuid(&self) -> &str;
    fn is_ready(&self) -> bool;
    /// Acquire a single page for the given scan settings.
    fn scan_page(&self, req: &ScanRequest) -> Result<ScanDocument, String>;
}

/// Pluggable backend for printing (mock today; USB Epson EcoTank tomorrow).
pub trait PrinterBackend: Send + Sync {
    fn make_and_model(&self) -> &str;
    fn printer_name(&self) -> &str;
    fn uuid(&self) -> &str;
    fn is_ready(&self) -> bool;
    /// Create a tracked job and return its ID.
    fn create_job(&self, name: String, format: String) -> PrintJobInfo;
    /// Receive raw print payload (URF / PWG-Raster / PDF / JPEG) for a job.
    fn submit_document(&self, job_id: u32, data: &[u8]) -> Result<PrintJobInfo, String>;
    fn get_job(&self, job_id: u32) -> Option<PrintJobInfo>;
    fn list_jobs(&self) -> Vec<PrintJobInfo>;
    fn cancel_job(&self, job_id: u32) -> Option<PrintJobInfo>;
}

/// Mock implementation that lets us verify macOS AirScan & AirPrint end-to-end
/// before plugging in the physical Epson EcoTank over USB.
pub struct MockEpsonBridge {
    next_print_job_id: AtomicU32,
    print_jobs: Mutex<Vec<PrintJobInfo>>,
}

impl MockEpsonBridge {
    pub fn new() -> Self {
        Self {
            next_print_job_id: AtomicU32::new(1),
            print_jobs: Mutex::new(Vec::new()),
        }
    }
}

use crate::config;

impl ScannerBackend for MockEpsonBridge {
    fn make_and_model(&self) -> &str {
        config::PRINTER_MODEL
    }

    fn serial_number(&self) -> &str {
        config::PRINTER_SERIAL
    }

    fn uuid(&self) -> &str {
        config::PRINTER_UUID
    }

    fn is_ready(&self) -> bool {
        true
    }

    fn scan_page(&self, req: &ScanRequest) -> Result<ScanDocument, String> {
        log::info!(
            "[MockScanner] Scanning platen page ({}x{} dpi, {}, {}, source={}) -> {} bytes JPEG",
            req.x_resolution,
            req.y_resolution,
            req.color_mode,
            req.document_format,
            req.input_source,
            TEST_SCAN_JPEG.len()
        );
        Ok(ScanDocument {
            mime_type: "image/jpeg",
            data: TEST_SCAN_JPEG.to_vec(),
        })
    }
}

impl PrinterBackend for MockEpsonBridge {
    fn make_and_model(&self) -> &str {
        config::PRINTER_MODEL
    }

    fn printer_name(&self) -> &str {
        config::PRINTER_NAME
    }

    fn uuid(&self) -> &str {
        config::PRINTER_UUID
    }

    fn is_ready(&self) -> bool {
        true
    }

    fn create_job(&self, name: String, format: String) -> PrintJobInfo {
        let id = self.next_print_job_id.fetch_add(1, Ordering::Relaxed);
        let job = PrintJobInfo {
            id,
            name,
            format,
            bytes_received: 0,
            state: PrintJobState::Pending,
        };
        let mut jobs = self.print_jobs.lock().unwrap();
        if jobs.len() >= 16 {
            jobs.remove(0);
        }
        jobs.push(job.clone());
        log::info!(
            "[MockPrinter] Created print job #{}: name=\"{}\" format={}",
            job.id,
            job.name,
            job.format
        );
        job
    }

    fn submit_document(&self, job_id: u32, data: &[u8]) -> Result<PrintJobInfo, String> {
        let mut jobs = self.print_jobs.lock().unwrap();
        let job = jobs
            .iter_mut()
            .find(|j| j.id == job_id)
            .ok_or_else(|| format!("Job #{job_id} not found"))?;
        job.bytes_received += data.len();
        job.state = PrintJobState::Completed;

        // Inspect magic header so we can see exactly what macOS/iOS sent (UNIRAST, %PDF, JPEG, etc.)
        let magic: String = data
            .iter()
            .take(8)
            .map(|&b| {
                if (0x20..=0x7e).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        log::info!(
            "[MockPrinter] Job #{} received {} bytes (header=\"{}\", format={}) -> marked Completed",
            job.id,
            data.len(),
            magic,
            job.format
        );
        Ok(job.clone())
    }

    fn get_job(&self, job_id: u32) -> Option<PrintJobInfo> {
        self.print_jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == job_id)
            .cloned()
    }

    fn list_jobs(&self) -> Vec<PrintJobInfo> {
        self.print_jobs.lock().unwrap().clone()
    }

    fn cancel_job(&self, job_id: u32) -> Option<PrintJobInfo> {
        let mut jobs = self.print_jobs.lock().unwrap();
        if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
            job.state = PrintJobState::Canceled;
            log::info!("[MockPrinter] Job #{} canceled", job_id);
            return Some(job.clone());
        }
        None
    }
}
