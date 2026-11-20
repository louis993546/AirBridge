use std::sync::Arc;

use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;

use crate::backend::{PrintJobInfo, PrinterBackend};
use crate::ipp_usb::IppUsbBridge;

// IPP Operation IDs (RFC 8011)
const OP_PRINT_JOB: u16 = 0x0002;
const OP_VALIDATE_JOB: u16 = 0x0004;
const OP_CREATE_JOB: u16 = 0x0005;
const OP_SEND_DOCUMENT: u16 = 0x0006;
const OP_CANCEL_JOB: u16 = 0x0008;
const OP_GET_JOB_ATTRIBUTES: u16 = 0x0009;
const OP_GET_JOBS: u16 = 0x000A;
const OP_GET_PRINTER_ATTRIBUTES: u16 = 0x000B;

// IPP Status Codes
const STATUS_OK: u16 = 0x0000;
const STATUS_CLIENT_ERROR_NOT_FOUND: u16 = 0x0406;
const STATUS_SERVER_ERROR_OPERATION_NOT_SUPPORTED: u16 = 0x0501;

// IPP Delimiter Tags (RFC 8010)
const TAG_OPERATION_ATTRIBUTES: u8 = 0x01;
const TAG_JOB_ATTRIBUTES: u8 = 0x02;
const TAG_END_OF_ATTRIBUTES: u8 = 0x03;
const TAG_PRINTER_ATTRIBUTES: u8 = 0x04;

// IPP Value Tags
const TAG_BEG_COLLECTION: u8 = 0x34;
const TAG_MEMBER_ATTR_NAME: u8 = 0x4A;
const TAG_END_COLLECTION: u8 = 0x37;
const TAG_INTEGER: u8 = 0x21;
const TAG_BOOLEAN: u8 = 0x22;
const TAG_ENUM: u8 = 0x23;
const TAG_RESOLUTION: u8 = 0x32;
const TAG_TEXT_WITHOUT_LANG: u8 = 0x41;
const TAG_NAME_WITHOUT_LANG: u8 = 0x42;
const TAG_KEYWORD: u8 = 0x44;
const TAG_URI: u8 = 0x45;
const TAG_CHARSET: u8 = 0x47;
const TAG_NATURAL_LANGUAGE: u8 = 0x48;
const TAG_MIME_MEDIA_TYPE: u8 = 0x49;

/// Minimal parsed IPP request header + extracted key attributes + offset where document payload starts.
struct ParsedIppRequest {
    version_major: u8,
    version_minor: u8,
    operation_id: u16,
    request_id: u32,
    job_id: Option<u32>,
    job_name: Option<String>,
    document_format: Option<String>,
    data_offset: usize,
}

fn parse_ipp_request(buf: &[u8]) -> Option<ParsedIppRequest> {
    if buf.len() < 9 {
        return None;
    }
    let version_major = buf[0];
    let version_minor = buf[1];
    let operation_id = u16::from_be_bytes([buf[2], buf[3]]);
    let request_id = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

    let mut pos = 8;
    let mut job_id = None;
    let mut job_name = None;
    let mut document_format = None;

    while pos < buf.len() {
        let tag = buf[pos];
        pos += 1;

        if tag == TAG_END_OF_ATTRIBUTES {
            break;
        }
        // Delimiter tags (0x00..=0x0F) start a new attribute group
        if tag < 0x10 {
            continue;
        }

        if pos + 2 > buf.len() {
            break;
        }
        let name_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
        pos += 2;
        if pos + name_len + 2 > buf.len() {
            break;
        }
        let name = core::str::from_utf8(&buf[pos..pos + name_len]).unwrap_or("");
        pos += name_len;

        let val_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
        pos += 2;
        if pos + val_len > buf.len() {
            break;
        }
        let val = &buf[pos..pos + val_len];
        pos += val_len;

        match name {
            "job-id" if val.len() == 4 => {
                job_id = Some(u32::from_be_bytes([val[0], val[1], val[2], val[3]]));
            }
            "job-name" => {
                job_name = Some(String::from_utf8_lossy(val).into_owned());
            }
            "document-format" => {
                document_format = Some(String::from_utf8_lossy(val).into_owned());
            }
            _ => {}
        }
    }

    Some(ParsedIppRequest {
        version_major,
        version_minor,
        operation_id,
        request_id,
        job_id,
        job_name,
        document_format,
        data_offset: pos,
    })
}

/// Binary IPP/2.0 response builder (RFC 8010).
struct IppResponse {
    buf: Vec<u8>,
}

impl IppResponse {
    fn new(version_major: u8, version_minor: u8, status_code: u16, request_id: u32) -> Self {
        let mut buf = Vec::with_capacity(512);
        buf.push(version_major);
        buf.push(version_minor);
        buf.extend_from_slice(&status_code.to_be_bytes());
        buf.extend_from_slice(&request_id.to_be_bytes());

        // Mandatory operation-attributes-tag with charset & natural language first
        buf.push(TAG_OPERATION_ATTRIBUTES);
        let mut r = Self { buf };
        r.attr_str(TAG_CHARSET, "attributes-charset", "utf-8");
        r.attr_str(TAG_NATURAL_LANGUAGE, "attributes-natural-language", "en");
        r
    }

    fn begin_group(&mut self, group_tag: u8) {
        self.buf.push(group_tag);
    }

    fn attr_raw(&mut self, value_tag: u8, name: &str, value: &[u8]) {
        self.buf.push(value_tag);
        self.buf
            .extend_from_slice(&(name.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(name.as_bytes());
        self.buf
            .extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(value);
    }

    fn attr_str(&mut self, value_tag: u8, name: &str, value: &str) {
        self.attr_raw(value_tag, name, value.as_bytes());
    }

    fn attr_str_list(&mut self, value_tag: u8, name: &str, values: &[&str]) {
        for (i, v) in values.iter().enumerate() {
            // Subsequent values in a 1setOf attribute use an empty name (name_len = 0)
            let attr_name = if i == 0 { name } else { "" };
            self.attr_str(value_tag, attr_name, v);
        }
    }

    fn attr_int(&mut self, value_tag: u8, name: &str, value: i32) {
        self.attr_raw(value_tag, name, &value.to_be_bytes());
    }

    fn attr_bool(&mut self, name: &str, value: bool) {
        self.attr_raw(TAG_BOOLEAN, name, &[if value { 1 } else { 0 }]);
    }

    fn attr_resolution(&mut self, name: &str, x_dpi: i32, y_dpi: i32) {
        let mut val = [0u8; 9];
        val[0..4].copy_from_slice(&x_dpi.to_be_bytes());
        val[4..8].copy_from_slice(&y_dpi.to_be_bytes());
        val[8] = 3; // 3 = dots per inch (RFC 8010)
        self.attr_raw(TAG_RESOLUTION, name, &val);
    }

    fn attr_media_col_a4(&mut self, name: &str) {
        // Begin outer media-col collection
        self.attr_raw(TAG_BEG_COLLECTION, name, &[]);
        // Member: media-size { x-dimension = 21000, y-dimension = 29700 } (hundredths of mm)
        self.attr_str(TAG_MEMBER_ATTR_NAME, "", "media-size");
        self.attr_raw(TAG_BEG_COLLECTION, "", &[]);
        self.attr_str(TAG_MEMBER_ATTR_NAME, "", "x-dimension");
        self.attr_int(TAG_INTEGER, "", 21000);
        self.attr_str(TAG_MEMBER_ATTR_NAME, "", "y-dimension");
        self.attr_int(TAG_INTEGER, "", 29700);
        self.attr_raw(TAG_END_COLLECTION, "", &[]);
        // Margins (3 mm = 300 hundredths of mm)
        for margin in [
            "media-bottom-margin",
            "media-left-margin",
            "media-right-margin",
            "media-top-margin",
        ] {
            self.attr_str(TAG_MEMBER_ATTR_NAME, "", margin);
            self.attr_int(TAG_INTEGER, "", 300);
        }
        self.attr_str(TAG_MEMBER_ATTR_NAME, "", "media-source");
        self.attr_str(TAG_KEYWORD, "", "main");
        self.attr_str(TAG_MEMBER_ATTR_NAME, "", "media-type");
        self.attr_str(TAG_KEYWORD, "", "stationery");
        // End outer collection
        self.attr_raw(TAG_END_COLLECTION, "", &[]);
    }

    fn finish(mut self) -> Vec<u8> {
        self.buf.push(TAG_END_OF_ATTRIBUTES);
        self.buf
    }
}

fn write_job_attributes(resp: &mut IppResponse, job: &PrintJobInfo, printer_uri: &str) {
    resp.begin_group(TAG_JOB_ATTRIBUTES);
    let job_uri = format!("{}/jobs/{}", printer_uri, job.id);
    resp.attr_str(TAG_URI, "job-uri", &job_uri);
    resp.attr_int(TAG_INTEGER, "job-id", job.id as i32);
    resp.attr_str(TAG_NAME_WITHOUT_LANG, "job-name", &job.name);
    resp.attr_int(TAG_ENUM, "job-state", job.state as i32);
    resp.attr_str(
        TAG_KEYWORD,
        "job-state-reasons",
        match job.state {
            crate::backend::PrintJobState::Completed => "job-completed-successfully",
            crate::backend::PrintJobState::Canceled => "job-canceled-by-user",
            _ => "none",
        },
    );
}

fn build_printer_attributes(
    printer: &dyn PrinterBackend,
    printer_uri: &str,
    req: &ParsedIppRequest,
) -> Vec<u8> {
    let mut resp = IppResponse::new(
        req.version_major,
        req.version_minor,
        STATUS_OK,
        req.request_id,
    );
    resp.begin_group(TAG_PRINTER_ATTRIBUTES);

    resp.attr_str(TAG_URI, "printer-uri-supported", printer_uri);
    resp.attr_str(TAG_KEYWORD, "uri-security-supported", "none");
    resp.attr_str(TAG_KEYWORD, "uri-authentication-supported", "none");
    resp.attr_str(TAG_NAME_WITHOUT_LANG, "printer-name", printer.printer_name());
    resp.attr_str(
        TAG_TEXT_WITHOUT_LANG,
        "printer-make-and-model",
        printer.make_and_model(),
    );
    resp.attr_str(
        TAG_TEXT_WITHOUT_LANG,
        "printer-info",
        "ESP32-P4 AirPrint & AirScan USB Bridge",
    );
    resp.attr_str(TAG_TEXT_WITHOUT_LANG, "printer-location", "Home Network");
    resp.attr_str(
        TAG_URI,
        "printer-uuid",
        &format!("urn:uuid:{}", printer.uuid()),
    );
    resp.attr_str(
        TAG_URI,
        "printer-more-info",
        "http://p4-airbridge.local/",
    );

    // 3 = idle, 5 = stopped
    let state = if printer.is_ready() { 3 } else { 5 };
    resp.attr_int(TAG_ENUM, "printer-state", state);
    resp.attr_str(TAG_KEYWORD, "printer-state-reasons", "none");
    resp.attr_bool("printer-is-accepting-jobs", true);
    resp.attr_int(TAG_INTEGER, "queued-job-count", 0);

    resp.attr_str_list(TAG_KEYWORD, "ipp-versions-supported", &["1.1", "2.0"]);
    // Operations supported
    let ops = [
        OP_PRINT_JOB,
        OP_VALIDATE_JOB,
        OP_CREATE_JOB,
        OP_SEND_DOCUMENT,
        OP_CANCEL_JOB,
        OP_GET_JOB_ATTRIBUTES,
        OP_GET_JOBS,
        OP_GET_PRINTER_ATTRIBUTES,
    ];
    for (i, &op) in ops.iter().enumerate() {
        resp.attr_int(
            TAG_ENUM,
            if i == 0 { "operations-supported" } else { "" },
            op as i32,
        );
    }

    resp.attr_str(TAG_CHARSET, "charset-configured", "utf-8");
    resp.attr_str(TAG_CHARSET, "charset-supported", "utf-8");
    resp.attr_str(
        TAG_NATURAL_LANGUAGE,
        "generated-natural-language-supported",
        "en",
    );
    resp.attr_str(TAG_NATURAL_LANGUAGE, "natural-language-configured", "en");

    resp.attr_str(TAG_MIME_MEDIA_TYPE, "document-format-default", "application/octet-stream");
    resp.attr_str_list(
        TAG_MIME_MEDIA_TYPE,
        "document-format-supported",
        &[
            "image/urf",
            "image/pwg-raster",
            "application/pdf",
            "image/jpeg",
            "application/octet-stream",
        ],
    );
    resp.attr_str_list(
        TAG_KEYWORD,
        "urf-supported",
        &["W8", "SRGB24", "RS300-600", "DM1", "PQ4"],
    );
    resp.attr_str_list(TAG_KEYWORD, "compression-supported", &["none"]);

    resp.attr_bool("color-supported", true);
    resp.attr_str(TAG_KEYWORD, "print-color-mode-default", "color");
    resp.attr_str_list(
        TAG_KEYWORD,
        "print-color-mode-supported",
        &["color", "monochrome", "auto"],
    );

    resp.attr_str(TAG_KEYWORD, "sides-default", "one-sided");
    resp.attr_str(TAG_KEYWORD, "sides-supported", "one-sided");

    resp.attr_str(TAG_KEYWORD, "media-default", "iso_a4_210x297mm");
    resp.attr_str_list(
        TAG_KEYWORD,
        "media-supported",
        &["iso_a4_210x297mm", "na_letter_8.5x11in"],
    );
    resp.attr_str_list(
        TAG_KEYWORD,
        "media-ready",
        &["iso_a4_210x297mm", "na_letter_8.5x11in"],
    );
    resp.attr_media_col_a4("media-col-default");
    resp.attr_media_col_a4("media-col-ready");
    resp.attr_str_list(
        TAG_KEYWORD,
        "media-col-supported",
        &[
            "media-bottom-margin",
            "media-left-margin",
            "media-right-margin",
            "media-size",
            "media-source",
            "media-top-margin",
            "media-type",
        ],
    );

    resp.attr_resolution("printer-resolution-default", 300, 300);
    resp.attr_resolution("printer-resolution-supported", 300, 300);
    resp.attr_resolution("", 600, 600);

    resp.attr_int(TAG_INTEGER, "printer-up-time", 100);
    resp.attr_str(TAG_KEYWORD, "pdl-override-supported", "attempted");

    resp.finish()
}

pub fn handle_ipp_body(
    printer: &dyn PrinterBackend,
    printer_uri: &str,
    body: &[u8],
) -> Vec<u8> {
    let Some(req) = parse_ipp_request(body) else {
        log::warn!("[IPP] Malformed IPP request ({} bytes)", body.len());
        return IppResponse::new(2, 0, 0x0400, 1).finish();
    };

    let payload = &body[req.data_offset.min(body.len())..];

    match req.operation_id {
        OP_GET_PRINTER_ATTRIBUTES => {
            log::info!(
                "[IPP] Get-Printer-Attributes (v{}.{}, req_id={})",
                req.version_major,
                req.version_minor,
                req.request_id
            );
            build_printer_attributes(printer, printer_uri, &req)
        }
        OP_VALIDATE_JOB => {
            log::info!("[IPP] Validate-Job (req_id={})", req.request_id);
            IppResponse::new(
                req.version_major,
                req.version_minor,
                STATUS_OK,
                req.request_id,
            )
            .finish()
        }
        OP_CREATE_JOB => {
            let name = req.job_name.unwrap_or_else(|| "AirPrint Job".to_string());
            let format = req
                .document_format
                .unwrap_or_else(|| "image/urf".to_string());
            let job = printer.create_job(name, format);
            let mut resp = IppResponse::new(
                req.version_major,
                req.version_minor,
                STATUS_OK,
                req.request_id,
            );
            write_job_attributes(&mut resp, &job, printer_uri);
            resp.finish()
        }
        OP_PRINT_JOB => {
            let name = req.job_name.unwrap_or_else(|| "AirPrint Job".to_string());
            let format = req
                .document_format
                .unwrap_or_else(|| "image/urf".to_string());
            let job = printer.create_job(name, format);
            let done = printer
                .submit_document(job.id, payload)
                .unwrap_or(job);
            let mut resp = IppResponse::new(
                req.version_major,
                req.version_minor,
                STATUS_OK,
                req.request_id,
            );
            write_job_attributes(&mut resp, &done, printer_uri);
            resp.finish()
        }
        OP_SEND_DOCUMENT => {
            let job_id = req.job_id.unwrap_or(1);
            match printer.submit_document(job_id, payload) {
                Ok(job) => {
                    let mut resp = IppResponse::new(
                        req.version_major,
                        req.version_minor,
                        STATUS_OK,
                        req.request_id,
                    );
                    write_job_attributes(&mut resp, &job, printer_uri);
                    resp.finish()
                }
                Err(_) => IppResponse::new(
                    req.version_major,
                    req.version_minor,
                    STATUS_CLIENT_ERROR_NOT_FOUND,
                    req.request_id,
                )
                .finish(),
            }
        }
        OP_GET_JOB_ATTRIBUTES => {
            let job_id = req.job_id.unwrap_or(1);
            log::info!("[IPP] Get-Job-Attributes (job_id={})", job_id);
            if let Some(job) = printer.get_job(job_id) {
                let mut resp = IppResponse::new(
                    req.version_major,
                    req.version_minor,
                    STATUS_OK,
                    req.request_id,
                );
                write_job_attributes(&mut resp, &job, printer_uri);
                resp.finish()
            } else {
                IppResponse::new(
                    req.version_major,
                    req.version_minor,
                    STATUS_CLIENT_ERROR_NOT_FOUND,
                    req.request_id,
                )
                .finish()
            }
        }
        OP_GET_JOBS => {
            log::info!("[IPP] Get-Jobs");
            let mut resp = IppResponse::new(
                req.version_major,
                req.version_minor,
                STATUS_OK,
                req.request_id,
            );
            for job in printer.list_jobs() {
                write_job_attributes(&mut resp, &job, printer_uri);
            }
            resp.finish()
        }
        OP_CANCEL_JOB => {
            let job_id = req.job_id.unwrap_or(1);
            if let Some(job) = printer.cancel_job(job_id) {
                let mut resp = IppResponse::new(
                    req.version_major,
                    req.version_minor,
                    STATUS_OK,
                    req.request_id,
                );
                write_job_attributes(&mut resp, &job, printer_uri);
                resp.finish()
            } else {
                IppResponse::new(
                    req.version_major,
                    req.version_minor,
                    STATUS_CLIENT_ERROR_NOT_FOUND,
                    req.request_id,
                )
                .finish()
            }
        }
        other => {
            log::warn!("[IPP] Unsupported operation 0x{:04X}", other);
            IppResponse::new(
                req.version_major,
                req.version_minor,
                STATUS_SERVER_ERROR_OPERATION_NOT_SUPPORTED,
                req.request_id,
            )
            .finish()
        }
    }
}

/// Register AirPrint IPP endpoint (`/ipp/print` and `/` POST) on the given HTTP server.
/// When a real IPP-USB printer is attached, streams requests & responses directly over USB;
/// otherwise falls back to the mock IPP/2.0 backend.
pub fn register_routes(
    server: &mut EspHttpServer<'static>,
    printer: Arc<dyn PrinterBackend>,
    ipp_usb: Arc<IppUsbBridge>,
    register_root_post: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    for path in if register_root_post {
        vec!["/ipp/print", "/"]
    } else {
        vec!["/ipp/print"]
    } {
        let p = printer.clone();
        let usb = ipp_usb.clone();
        server.fn_handler(path, Method::Post, move |mut req| {
            // Fast path: stream directly to real USB printer over IPP-USB!
            if usb.is_connected() {
                let uri = if req.uri() == "/" {
                    "/ipp/print".to_string()
                } else {
                    req.uri().to_string()
                };
                let content_type = req
                    .header("Content-Type")
                    .unwrap_or("application/ipp")
                    .to_string();
                let content_length = req
                    .header("Content-Length")
                    .and_then(|s| s.trim().parse::<usize>().ok());

                log::info!(
                    "[IPP->USB] POST {} (ct={}, len={:?})",
                    uri,
                    content_type,
                    content_length
                );

                let res = usb.exchange_stream_both(
                    "POST",
                    &uri,
                    Some(content_type.as_str()),
                    content_length,
                    req,
                    |r, buf| r.read(buf).map_err(|e| format!("http read err: {:?}", e)),
                    |r, head| {
                        let mut owned_headers: Vec<(String, String)> = Vec::new();
                        for (k, v) in &head.headers {
                            if k.eq_ignore_ascii_case("Content-Type")
                                || (k.eq_ignore_ascii_case("Content-Length") && !head.chunked)
                            {
                                owned_headers.push((k.clone(), v.clone()));
                            }
                        }
                        if !owned_headers
                            .iter()
                            .any(|(k, _)| k.eq_ignore_ascii_case("Content-Type"))
                        {
                            owned_headers
                                .push(("Content-Type".to_string(), "application/ipp".to_string()));
                        }
                        let hdr_refs: Vec<(&str, &str)> = owned_headers
                            .iter()
                            .map(|(k, v)| (k.as_str(), v.as_str()))
                            .collect();
                        r.into_response(
                            head.status_code,
                            Some(head.status_text.as_str()),
                            &hdr_refs,
                        )
                        .map_err(|e| format!("into_response err: {:?}", e))
                    },
                    |resp, chunk| {
                        resp.write_all(chunk)
                            .map_err(|e| format!("http write err: {:?}", e))?;
                        Ok(())
                    },
                );

                match res {
                    Ok(mut resp) => {
                        let _ = resp.flush();
                    }
                    Err(msg) => {
                        log::error!("[IPP->USB] Failed: {}", msg);
                    }
                }
                return Ok::<(), esp_idf_svc::io::EspIOError>(());
            }

            // Mock fallback when no USB printer is connected
            let host = req
                .header("Host")
                .unwrap_or("p4-airbridge.local:631")
                .to_string();
            let printer_uri = format!("ipp://{}/ipp/print", host);

            let mut body = Vec::new();
            let mut chunk = [0u8; 4096];
            const MAX_IPP_BODY: usize = 4 * 1024 * 1024;
            while body.len() < MAX_IPP_BODY {
                match req.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }

            let resp_bytes = handle_ipp_body(p.as_ref(), &printer_uri, &body);
            let len_str = resp_bytes.len().to_string();
            let mut resp = req.into_response(
                200,
                Some("OK"),
                &[
                    ("Content-Type", "application/ipp"),
                    ("Content-Length", len_str.as_str()),
                ],
            )?;
            resp.write_all(&resp_bytes)?;
            resp.flush()?;
            Ok::<(), esp_idf_svc::io::EspIOError>(())
        })?;
    }

    Ok(())
}
