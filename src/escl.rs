use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use esp_idf_svc::http::server::{EspHttpServer, Request};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;

use crate::backend::{ScanDocument, ScanRequest, ScannerBackend};
use crate::ipp_usb::{rewrite_localhost_url, IppUsbBridge};
use crate::scan_cal::ScanCalStore;

#[derive(Debug, Clone)]
struct ScanJob {
    id: u32,
    #[allow(dead_code)]
    request: ScanRequest,
    document: Option<ScanDocument>,
    completed: bool,
}

pub struct EsclState {
    backend: Arc<dyn ScannerBackend>,
    ipp_usb: Arc<IppUsbBridge>,
    scan_cal: Arc<ScanCalStore>,
    next_job_id: AtomicU32,
    jobs: Mutex<HashMap<u32, ScanJob>>,
}

impl EsclState {
    pub fn new(
        backend: Arc<dyn ScannerBackend>,
        ipp_usb: Arc<IppUsbBridge>,
        scan_cal: Arc<ScanCalStore>,
    ) -> Self {
        Self {
            backend,
            ipp_usb,
            scan_cal,
            next_job_id: AtomicU32::new(1),
            jobs: Mutex::new(HashMap::new()),
        }
    }

    fn active_job_count(&self) -> usize {
        self.jobs
            .lock()
            .unwrap()
            .values()
            .filter(|j| !j.completed)
            .count()
    }

    fn capabilities_xml(&self) -> String {
        let model = self.backend.make_and_model();
        let serial = self.backend.serial_number();
        let uuid = self.backend.uuid();

        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScannerCapabilities xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03" xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm">
  <pwg:Version>2.63</pwg:Version>
  <pwg:MakeAndModel>{model}</pwg:MakeAndModel>
  <pwg:SerialNumber>{serial}</pwg:SerialNumber>
  <scan:UUID>{uuid}</scan:UUID>
  <scan:AdminURI>http://p4-airbridge.local/</scan:AdminURI>
  <scan:Platen>
    <scan:PlatenInputCaps>
      <scan:MinWidth>16</scan:MinWidth>
      <scan:MaxWidth>2550</scan:MaxWidth>
      <scan:MinHeight>16</scan:MinHeight>
      <scan:MaxHeight>3508</scan:MaxHeight>
      <scan:MaxScanRegions>1</scan:MaxScanRegions>
      <scan:SettingProfiles>
        <scan:SettingProfile>
          <scan:ColorModes>
            <scan:ColorMode>RGB24</scan:ColorMode>
            <scan:ColorMode>Grayscale8</scan:ColorMode>
          </scan:ColorModes>
          <scan:DocumentFormats>
            <pwg:DocumentFormat>image/jpeg</pwg:DocumentFormat>
            <scan:DocumentFormatExt>image/jpeg</scan:DocumentFormatExt>
          </scan:DocumentFormats>
          <scan:SupportedResolutions>
            <scan:DiscreteResolutions>
              <scan:DiscreteResolution>
                <scan:XResolution>75</scan:XResolution>
                <scan:YResolution>75</scan:YResolution>
              </scan:DiscreteResolution>
              <scan:DiscreteResolution>
                <scan:XResolution>150</scan:XResolution>
                <scan:YResolution>150</scan:YResolution>
              </scan:DiscreteResolution>
              <scan:DiscreteResolution>
                <scan:XResolution>300</scan:XResolution>
                <scan:YResolution>300</scan:YResolution>
              </scan:DiscreteResolution>
              <scan:DiscreteResolution>
                <scan:XResolution>600</scan:XResolution>
                <scan:YResolution>600</scan:YResolution>
              </scan:DiscreteResolution>
            </scan:DiscreteResolutions>
          </scan:SupportedResolutions>
          <scan:ColorSpaces>
            <scan:ColorSpace>sRGB</scan:ColorSpace>
          </scan:ColorSpaces>
        </scan:SettingProfile>
      </scan:SettingProfiles>
      <scan:SupportedIntents>
        <scan:Intent>Document</scan:Intent>
        <scan:Intent>Photo</scan:Intent>
        <scan:Intent>Preview</scan:Intent>
        <scan:Intent>TextAndGraphic</scan:Intent>
      </scan:SupportedIntents>
      <scan:MaxOpticalXResolution>600</scan:MaxOpticalXResolution>
      <scan:MaxOpticalYResolution>600</scan:MaxOpticalYResolution>
      <scan:RiskyLeftMargin>0</scan:RiskyLeftMargin>
      <scan:RiskyRightMargin>0</scan:RiskyRightMargin>
      <scan:RiskyTopMargin>0</scan:RiskyTopMargin>
      <scan:RiskyBottomMargin>0</scan:RiskyBottomMargin>
    </scan:PlatenInputCaps>
  </scan:Platen>
  <scan:CompressionFactorSupport>
    <scan:Min>1</scan:Min>
    <scan:Max>100</scan:Max>
    <scan:Normal>75</scan:Normal>
    <scan:Step>1</scan:Step>
  </scan:CompressionFactorSupport>
</scan:ScannerCapabilities>
"#
        )
    }

    fn status_xml(&self) -> String {
        let scanner_state = if !self.backend.is_ready() {
            "Stopped"
        } else if self.active_job_count() > 0 {
            "Processing"
        } else {
            "Idle"
        };

        let jobs_xml: String = {
            let jobs = self.jobs.lock().unwrap();
            jobs.values()
                .map(|j| {
                    let job_state = if j.completed { "Completed" } else { "Processing" };
                    let reason = if j.completed {
                        "JobCompletedSuccessfully"
                    } else {
                        "JobScanning"
                    };
                    let remaining = if j.document.is_some() { 1 } else { 0 };
                    let completed = if j.completed { 1 } else { 0 };
                    format!(
                        r#"    <scan:JobInfo>
      <pwg:JobUri>/eSCL/ScanJobs/{id}</pwg:JobUri>
      <pwg:JobUuid>{id}</pwg:JobUuid>
      <scan:Age>1</scan:Age>
      <pwg:ImagesCompleted>{completed}</pwg:ImagesCompleted>
      <pwg:ImagesToTransfer>{remaining}</pwg:ImagesToTransfer>
      <pwg:JobState>{job_state}</pwg:JobState>
      <pwg:JobStateReasons>
        <pwg:JobStateReason>{reason}</pwg:JobStateReason>
      </pwg:JobStateReasons>
    </scan:JobInfo>
"#,
                        id = j.id
                    )
                })
                .collect()
        };

        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScannerStatus xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03" xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm">
  <pwg:Version>2.63</pwg:Version>
  <pwg:State>{scanner_state}</pwg:State>
  <scan:Jobs>
{jobs_xml}  </scan:Jobs>
</scan:ScannerStatus>
"#
        )
    }
}

/// Forward an eSCL HTTP request directly to the USB-connected printer over IPP-USB,
/// streaming the response body (e.g. multi-megabyte scanned JPEG/PDF) straight to the client.
fn proxy_escl_to_usb<C>(
    ipp_usb: &IppUsbBridge,
    method: &str,
    req: Request<C>,
    req_body: &[u8],
) -> Result<(), esp_idf_svc::io::EspIOError>
where
    C: esp_idf_svc::http::server::Connection,
    esp_idf_svc::io::EspIOError: From<C::Error>,
{
    let uri = req.uri().to_string();
    let host = req.header("Host").unwrap_or("p4-airbridge.local").to_string();
    let content_type = req.header("Content-Type").map(|s| s.to_string());

    log::info!(
        "[eSCL->USB] {} {} ({} req bytes)",
        method,
        uri,
        req_body.len()
    );

    let mut req_opt = Some(req);
    let mut streamed_bytes = 0usize;

    let res = ipp_usb.exchange_streaming(
        method,
        &uri,
        content_type.as_deref(),
        req_body,
        |head| {
            let req = req_opt.take().ok_or("request already consumed")?;
            let mut owned_headers: Vec<(String, String)> = Vec::new();
            for (k, v) in &head.headers {
                if k.eq_ignore_ascii_case("Content-Type")
                    || k.eq_ignore_ascii_case("Cache-Control")
                    || k.eq_ignore_ascii_case("Pragma")
                    || k.eq_ignore_ascii_case("Content-Disposition")
                {
                    owned_headers.push((k.clone(), v.clone()));
                } else if k.eq_ignore_ascii_case("Location")
                    || k.eq_ignore_ascii_case("Content-Location")
                {
                    let rewritten = rewrite_localhost_url(v, &host);
                    log::info!("[eSCL->USB] Rewrite {}: {} -> {}", k, v, rewritten);
                    owned_headers.push((k.clone(), rewritten));
                } else if k.eq_ignore_ascii_case("Content-Length") && !head.chunked {
                    owned_headers.push((k.clone(), v.clone()));
                }
            }
            if head.status_code == 201
                && !owned_headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
            {
                owned_headers.push(("Content-Length".to_string(), "0".to_string()));
            }

            let hdr_refs: Vec<(&str, &str)> = owned_headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();

            log::info!(
                "[eSCL<-USB] {} {} -> HTTP {} {}",
                method,
                uri,
                head.status_code,
                head.status_text
            );

            req.into_response(head.status_code, Some(head.status_text.as_str()), &hdr_refs)
                .map_err(|e| format!("into_response failed: {:?}", e))
        },
        |resp, chunk| {
            resp.write_all(chunk)
                .map_err(|e| format!("http write failed: {:?}", e))?;
            streamed_bytes += chunk.len();
            Ok(())
        },
    );

    match res {
        Ok(mut resp) => {
            let _ = resp.flush();
            if streamed_bytes > 0 {
                log::info!(
                    "[eSCL<-USB] {} {} streamed {} bytes",
                    method,
                    uri,
                    streamed_bytes
                );
            }
        }
        Err(msg) => {
            log::error!("[eSCL->USB] Failed: {}", msg);
            if let Some(req) = req_opt {
                let mut resp = req.into_response(
                    502,
                    Some("Bad Gateway"),
                    &[("Content-Type", "text/plain")],
                )?;
                resp.write_all(msg.as_bytes())?;
            }
        }
    }

    Ok(())
}

/// Register all AirScan (eSCL) routes on the given HTTP server.
pub fn register_routes(
    server: &mut EspHttpServer<'static>,
    state: Arc<EsclState>,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. GET /eSCL/ScannerCapabilities
    {
        let st = state.clone();
        server.fn_handler("/eSCL/ScannerCapabilities", Method::Get, move |req| {
            if st.ipp_usb.is_connected() {
                return proxy_escl_to_usb(&st.ipp_usb, "GET", req, &[]);
            }
            log::info!("[eSCL Mock] GET /eSCL/ScannerCapabilities");
            let xml = st.capabilities_xml();
            let mut resp = req.into_response(
                200,
                Some("OK"),
                &[
                    ("Content-Type", "text/xml; charset=utf-8"),
                    ("Cache-Control", "no-cache"),
                ],
            )?;
            resp.write_all(xml.as_bytes())?;
            Ok::<(), esp_idf_svc::io::EspIOError>(())
        })?;
    }

    // 2. GET /eSCL/ScannerStatus
    {
        let st = state.clone();
        server.fn_handler("/eSCL/ScannerStatus", Method::Get, move |req| {
            if st.ipp_usb.is_connected() {
                return proxy_escl_to_usb(&st.ipp_usb, "GET", req, &[]);
            }
            log::info!("[eSCL Mock] GET /eSCL/ScannerStatus");
            let xml = st.status_xml();
            let mut resp = req.into_response(
                200,
                Some("OK"),
                &[
                    ("Content-Type", "text/xml; charset=utf-8"),
                    ("Cache-Control", "no-cache"),
                ],
            )?;
            resp.write_all(xml.as_bytes())?;
            Ok::<(), esp_idf_svc::io::EspIOError>(())
        })?;
    }

    // 3. POST /eSCL/ScanJobs
    {
        let st = state.clone();
        server.fn_handler("/eSCL/ScanJobs", Method::Post, move |mut req| {
            let body_bytes = read_request_body_bytes(&mut req, 16384);
            if st.ipp_usb.is_connected() {
                let tuned_body = st.scan_cal.rewrite_scan_settings(&body_bytes);
                return proxy_escl_to_usb(&st.ipp_usb, "POST", req, &tuned_body);
            }

            let body = String::from_utf8_lossy(&body_bytes);
            let scan_req = parse_scan_settings(&body);
            let id = st.next_job_id.fetch_add(1, Ordering::Relaxed);

            log::info!(
                "[eSCL Mock] POST /eSCL/ScanJobs -> job #{} ({}x{} dpi, {}, {})",
                id,
                scan_req.x_resolution,
                scan_req.y_resolution,
                scan_req.color_mode,
                scan_req.document_format
            );

            match st.backend.scan_page(&scan_req) {
                Ok(doc) => {
                    let mut jobs = st.jobs.lock().unwrap();
                    if jobs.len() >= 8 {
                        if let Some(&oldest) = jobs.keys().min() {
                            jobs.remove(&oldest);
                        }
                    }
                    jobs.insert(
                        id,
                        ScanJob {
                            id,
                            request: scan_req,
                            document: Some(doc),
                            completed: false,
                        },
                    );

                    let location = if let Some(host) = req.header("Host") {
                        format!("http://{}/eSCL/ScanJobs/{}", host, id)
                    } else {
                        format!("/eSCL/ScanJobs/{}", id)
                    };

                    let mut resp = req.into_response(
                        201,
                        Some("Created"),
                        &[("Location", location.as_str()), ("Content-Length", "0")],
                    )?;
                    resp.flush()?;
                }
                Err(err) => {
                    let mut resp = req.into_response(
                        503,
                        Some("Service Unavailable"),
                        &[("Content-Type", "text/plain")],
                    )?;
                    resp.write_all(err.as_bytes())?;
                }
            }
            Ok::<(), esp_idf_svc::io::EspIOError>(())
        })?;
    }

    // 4. GET /eSCL/ScanJobs/* — handles NextDocument & ScanImageInfo
    {
        let st = state.clone();
        server.fn_handler("/eSCL/ScanJobs/*", Method::Get, move |req| {
            if st.ipp_usb.is_connected() {
                return proxy_escl_to_usb(&st.ipp_usb, "GET", req, &[]);
            }

            let uri = req.uri().to_string();
            log::info!("[eSCL Mock] GET {}", uri);

            let Some(id) = extract_job_id(&uri) else {
                let mut resp = req.into_response(404, Some("Not Found"), &[])?;
                resp.flush()?;
                return Ok::<(), esp_idf_svc::io::EspIOError>(());
            };

            if uri.ends_with("/NextDocument") {
                let doc_opt = {
                    let mut jobs = st.jobs.lock().unwrap();
                    if let Some(job) = jobs.get_mut(&id) {
                        let doc = job.document.take();
                        if doc.is_none() {
                            job.completed = true;
                        }
                        Some(doc)
                    } else {
                        None
                    }
                };

                match doc_opt {
                    Some(Some(doc)) => {
                        let len_str = doc.data.len().to_string();
                        let mut resp = req.into_response(
                            200,
                            Some("OK"),
                            &[
                                ("Content-Type", doc.mime_type),
                                ("Content-Length", len_str.as_str()),
                                ("Content-Location", uri.as_str()),
                            ],
                        )?;
                        for chunk in doc.data.chunks(4096) {
                            resp.write_all(chunk)?;
                        }
                        resp.flush()?;
                        if let Some(job) = st.jobs.lock().unwrap().get_mut(&id) {
                            job.completed = true;
                        }
                    }
                    _ => {
                        let mut resp = req.into_response(404, Some("Not Found"), &[])?;
                        resp.flush()?;
                    }
                }
            } else {
                let mut resp = req.into_response(404, Some("Not Found"), &[])?;
                resp.flush()?;
            }

            Ok(())
        })?;
    }

    // 5. DELETE /eSCL/ScanJobs/* — cancel/delete job
    {
        let st = state.clone();
        server.fn_handler("/eSCL/ScanJobs/*", Method::Delete, move |req| {
            if st.ipp_usb.is_connected() {
                return proxy_escl_to_usb(&st.ipp_usb, "DELETE", req, &[]);
            }
            let uri = req.uri().to_string();
            log::info!("[eSCL Mock] DELETE {}", uri);
            if let Some(id) = extract_job_id(&uri) {
                let removed = st.jobs.lock().unwrap().remove(&id).is_some();
                let status = if removed { 200 } else { 404 };
                let msg = if removed { "OK" } else { "Not Found" };
                let mut resp = req.into_response(status, Some(msg), &[])?;
                resp.flush()?;
            } else {
                let mut resp = req.into_response(404, Some("Not Found"), &[])?;
                resp.flush()?;
            }
            Ok::<(), esp_idf_svc::io::EspIOError>(())
        })?;
    }

    // 6. Catch-all GET /eSCL/* (e.g. icons or Epson-specific eSCL resources)
    {
        let st = state.clone();
        server.fn_handler("/eSCL/*", Method::Get, move |req| {
            if st.ipp_usb.is_connected() {
                return proxy_escl_to_usb(&st.ipp_usb, "GET", req, &[]);
            }
            let mut resp = req.into_response(404, Some("Not Found"), &[])?;
            resp.flush()?;
            Ok::<(), esp_idf_svc::io::EspIOError>(())
        })?;
    }

    Ok(())
}

fn extract_job_id(uri: &str) -> Option<u32> {
    let rest = uri.strip_prefix("/eSCL/ScanJobs/")?;
    let id_part = rest.split('/').next()?;
    id_part.parse::<u32>().ok()
}

fn read_request_body_bytes<C>(req: &mut Request<C>, max_len: usize) -> Vec<u8>
where
    C: esp_idf_svc::http::server::Connection,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    while buf.len() < max_len {
        match req.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    buf
}

fn parse_scan_settings(xml: &str) -> ScanRequest {
    let mut req = ScanRequest::default();
    if let Some(v) = extract_xml_tag(xml, "XResolution") {
        if let Ok(n) = v.trim().parse() {
            req.x_resolution = n;
        }
    }
    if let Some(v) = extract_xml_tag(xml, "YResolution") {
        if let Ok(n) = v.trim().parse() {
            req.y_resolution = n;
        }
    }
    if let Some(v) = extract_xml_tag(xml, "ColorMode") {
        req.color_mode = v.trim().to_string();
    }
    if let Some(v) = extract_xml_tag(xml, "DocumentFormat")
        .or_else(|| extract_xml_tag(xml, "DocumentFormatExt"))
    {
        req.document_format = v.trim().to_string();
    }
    if let Some(v) = extract_xml_tag(xml, "InputSource") {
        req.input_source = v.trim().to_string();
    }
    req
}

fn extract_xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open_suffix = format!(":{}>", tag);
    let open_plain = format!("<{}>", tag);

    let start_idx = if let Some(pos) = xml.find(&open_suffix) {
        pos + open_suffix.len()
    } else if let Some(pos) = xml.find(&open_plain) {
        pos + open_plain.len()
    } else {
        return None;
    };

    let tail = &xml[start_idx..];
    let end_idx = tail.find("</")?;
    Some(&tail[..end_idx])
}
