use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::backend::PrinterBackend;
use crate::ipp::handle_ipp_body;
use crate::ipp_usb::IppUsbBridge;

const MAX_CONCURRENT_IPP_CONNS: usize = 5;

struct ConnGuard(Arc<AtomicUsize>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Start a dedicated pure-Rust HTTP/1.1 AirPrint IPP server on port 631.
/// Unlike ESP-IDF's `esp_http_server`, this natively supports:
/// - `Expect: 100-continue` (required by macOS CUPS / AirPrint)
/// - `Transfer-Encoding: chunked` request bodies (required by CUPS Print-Job / Send-Document)
/// - HTTP/1.1 Keep-Alive persistent connections
/// - Automatic IPP URI sanitization (fixes Epson ET-2720's `http://localhost/...` bug!)
pub fn start_ipp_tcp_server(
    port: u16,
    printer: Arc<dyn PrinterBackend>,
    ipp_usb: Arc<IppUsbBridge>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    log::info!(
        "[IPP-TCP] AirPrint HTTP/1.1 server listening on 0.0.0.0:{} (chunked + 100-continue + keep-alive)",
        port
    );

    let active_conns = Arc::new(AtomicUsize::new(0));

    thread::Builder::new()
        .name("ipp_tcp_accept".into())
        .stack_size(8192)
        .spawn(move || {
            for incoming in listener.incoming() {
                match incoming {
                    Ok(mut stream) => {
                        let prev = active_conns.fetch_add(1, Ordering::SeqCst);
                        if prev >= MAX_CONCURRENT_IPP_CONNS {
                            active_conns.fetch_sub(1, Ordering::SeqCst);
                            log::warn!("[IPP-TCP] Max concurrent connections ({}) reached; sending 503", MAX_CONCURRENT_IPP_CONNS);
                            let _ = stream.write_all(
                                b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                            continue;
                        }
                        let guard = ConnGuard(active_conns.clone());
                        let p = printer.clone();
                        let usb = ipp_usb.clone();
                        if let Err(e) = thread::Builder::new()
                            .name("ipp_conn".into())
                            .stack_size(16384)
                            .spawn(move || {
                                let _guard = guard;
                                if let Err(e) = handle_connection(stream, p, usb) {
                                    log::debug!("[IPP-TCP] Connection closed: {}", e);
                                }
                            })
                        {
                            log::error!("[IPP-TCP] Failed to spawn handler thread: {:?}", e);
                        }
                    }
                    Err(e) => {
                        log::warn!("[IPP-TCP] accept error: {}", e);
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        })?;

    Ok(())
}

struct ParsedHttpReqHead {
    method: String,
    uri: String,
    host: String,
    content_type: String,
    content_length: Option<usize>,
    chunked: bool,
    expect_100: bool,
    keep_alive: bool,
}

fn read_http_request_head(reader: &mut BufReader<TcpStream>) -> Result<Option<ParsedHttpReqHead>, String> {
    let mut request_line = String::new();
    match reader.read_line(&mut request_line) {
        Ok(0) => return Ok(None), // Clean EOF
        Ok(_) => {}
        Err(e) => return Err(format!("read request line: {}", e)),
    }
    let trimmed = request_line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let mut parts = trimmed.split_whitespace();
    let method = parts.next().unwrap_or("POST").to_string();
    let uri = parts.next().unwrap_or("/ipp/print").to_string();
    let version = parts.next().unwrap_or("HTTP/1.1");

    let mut host = "p4-airbridge.local:631".to_string();
    let mut content_type = "application/ipp".to_string();
    let mut content_length = None;
    let mut chunked = false;
    let mut expect_100 = false;
    let mut keep_alive = version.eq_ignore_ascii_case("HTTP/1.1");

    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("read header: {}", e))?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if key.eq_ignore_ascii_case("Host") {
                host = val.to_string();
            } else if key.eq_ignore_ascii_case("Content-Type") {
                content_type = val.to_string();
            } else if key.eq_ignore_ascii_case("Content-Length") {
                content_length = val.parse::<usize>().ok();
            } else if key.eq_ignore_ascii_case("Transfer-Encoding")
                && val.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            } else if key.eq_ignore_ascii_case("Expect")
                && val.to_ascii_lowercase().contains("100-continue")
            {
                expect_100 = true;
            } else if key.eq_ignore_ascii_case("Connection") {
                let lower = val.to_ascii_lowercase();
                if lower.contains("close") {
                    keep_alive = false;
                } else if lower.contains("keep-alive") {
                    keep_alive = true;
                }
            }
        }
    }

    Ok(Some(ParsedHttpReqHead {
        method,
        uri,
        host,
        content_type,
        content_length,
        chunked,
        expect_100,
        keep_alive,
    }))
}

/// Reader adapter that yields decoded HTTP request body bytes from either
/// `Content-Length` or `Transfer-Encoding: chunked`.
enum HttpBodyReader<'a> {
    Fixed {
        reader: &'a mut BufReader<TcpStream>,
        remaining: usize,
    },
    Chunked {
        reader: &'a mut BufReader<TcpStream>,
        chunk_rem: usize,
        done: bool,
    },
}

impl<'a> HttpBodyReader<'a> {
    fn read_chunk(&mut self, out: &mut [u8]) -> Result<usize, String> {
        match self {
            HttpBodyReader::Fixed { reader, remaining } => {
                if *remaining == 0 {
                    return Ok(0);
                }
                let to_read = (*remaining).min(out.len());
                let n = reader
                    .read(&mut out[..to_read])
                    .map_err(|e| format!("body read: {}", e))?;
                *remaining -= n;
                Ok(n)
            }
            HttpBodyReader::Chunked {
                reader,
                chunk_rem,
                done,
            } => {
                if *done {
                    return Ok(0);
                }
                while *chunk_rem == 0 {
                    let mut size_line = String::new();
                    let n = reader
                        .read_line(&mut size_line)
                        .map_err(|e| format!("chunk size read: {}", e))?;
                    if n == 0 {
                        *done = true;
                        return Ok(0);
                    }
                    let hex = size_line.split(';').next().unwrap_or("").trim();
                    if hex.is_empty() {
                        continue;
                    }
                    let sz = usize::from_str_radix(hex, 16)
                        .map_err(|_| format!("bad chunk size: {:?}", size_line))?;
                    if sz == 0 {
                        // Drain optional trailers + trailing empty line
                        loop {
                            let mut trailer = String::new();
                            let m = reader.read_line(&mut trailer).unwrap_or(0);
                            if m == 0 || trailer == "\r\n" || trailer == "\n" {
                                break;
                            }
                        }
                        *done = true;
                        return Ok(0);
                    }
                    *chunk_rem = sz;
                }

                let to_read = (*chunk_rem).min(out.len());
                let n = reader
                    .read(&mut out[..to_read])
                    .map_err(|e| format!("chunk data read: {}", e))?;
                if n == 0 {
                    *done = true;
                    return Ok(0);
                }
                *chunk_rem -= n;
                if *chunk_rem == 0 {
                    // Consume chunk-ending \r\n
                    let mut crlf = [0u8; 2];
                    let _ = reader.read_exact(&mut crlf);
                }
                Ok(n)
            }
        }
    }
}

fn handle_connection(
    stream: TcpStream,
    printer: Arc<dyn PrinterBackend>,
    ipp_usb: Arc<IppUsbBridge>,
) -> Result<(), String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);

    // Note: do NOT call stream.try_clone() — ESP-IDF lwIP sockets do not support dup().
    // Access the underlying TcpStream for writing via reader.get_mut() between reads.
    let mut reader = BufReader::with_capacity(4096, stream);

    loop {
        let Some(req) = read_http_request_head(&mut reader)? else {
            return Ok(());
        };

        log::info!(
            "[IPP-TCP] {} {} (host={}, ct={}, len={:?}, chunked={}, 100c={})",
            req.method,
            req.uri,
            req.host,
            req.content_type,
            req.content_length,
            req.chunked,
            req.expect_100
        );

        // Immediately grant 100 Continue so macOS CUPS doesn't stall waiting!
        if req.expect_100 {
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .map_err(|e| format!("write 100 continue: {}", e))?;
            reader.get_mut().flush().ok();
        }

        // Normalize target URI for Epson IPP-USB
        let target_uri = if req.uri == "/" && req.method == "POST" {
            "/ipp/print".to_string()
        } else {
            req.uri.clone()
        };

        // Hostname without port (for http:// URLs on port 80) and with port (for ipp:// URLs on 631)
        let host_no_port = req
            .host
            .split(':')
            .next()
            .unwrap_or("p4-airbridge.local")
            .to_string();
        let host_ipp = if req.host.contains(':') {
            req.host.clone()
        } else {
            format!("{}:631", host_no_port)
        };

        let mut body_reader = if req.chunked {
            HttpBodyReader::Chunked {
                reader: &mut reader,
                chunk_rem: 0,
                done: false,
            }
        } else {
            HttpBodyReader::Fixed {
                reader: &mut reader,
                remaining: req.content_length.unwrap_or(0),
            }
        };

        if ipp_usb.is_connected() {
            // Stream request body directly to Epson ET-2720 over High-Speed IPP-USB,
            // buffer the (small) IPP response so we can sanitize Epson's broken localhost URIs,
            // and reply with an exact Content-Length!
            let usb_res = ipp_usb.exchange_stream_both(
                &req.method,
                &target_uri,
                if req.method == "POST" {
                    Some(req.content_type.as_str())
                } else {
                    None
                },
                if req.chunked { None } else { req.content_length },
                body_reader,
                |br, buf| br.read_chunk(buf),
                |_br, head| Ok((head.clone(), Vec::with_capacity(4096))),
                |(_, resp_buf), chunk| {
                    resp_buf.extend_from_slice(chunk);
                    Ok(())
                },
            );

            match usb_res {
                Ok((head, raw_body)) => {
                    let resp_ct = head
                        .header("Content-Type")
                        .unwrap_or("application/ipp")
                        .to_string();

                    // Fix Epson ET-2720 IPP-USB bug where `printer-more-info` & `printer-strings-uri`
                    // contain garbage bytes (`http://localhost\xXX/...` or `http://localhost /...`)
                    // and rewrite `localhost` -> our ESP32-P4 host!
                    let final_body = if resp_ct.to_ascii_lowercase().contains("application/ipp") {
                        sanitize_ipp_response(&raw_body, &host_no_port, &host_ipp)
                    } else {
                        raw_body
                    };

                    let conn_hdr = if req.keep_alive { "keep-alive" } else { "close" };
                    let resp_head = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
                        head.status_code,
                        head.status_text,
                        resp_ct,
                        final_body.len(),
                        conn_hdr
                    );
                    log::info!(
                        "[IPP-TCP<-USB] {} {} -> HTTP {} ({} bytes)",
                        req.method,
                        target_uri,
                        head.status_code,
                        final_body.len()
                    );
                    reader
                        .get_mut()
                        .write_all(resp_head.as_bytes())
                        .map_err(|e| format!("write resp head: {}", e))?;
                    reader
                        .get_mut()
                        .write_all(&final_body)
                        .map_err(|e| format!("write resp body: {}", e))?;
                    reader.get_mut().flush().ok();
                }
                Err(e) => {
                    log::error!("[IPP-TCP->USB] Error: {}", e);
                    let err_resp = format!(
                        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        e.len(),
                        e
                    );
                    let _ = reader.get_mut().write_all(err_resp.as_bytes());
                    let _ = reader.get_mut().flush();
                    return Ok(());
                }
            }
        } else {
            // Mock fallback when USB printer is not connected
            let mut body = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = body_reader.read_chunk(&mut tmp)?;
                if n == 0 {
                    break;
                }
                if body.len() < 4 * 1024 * 1024 {
                    body.extend_from_slice(&tmp[..n]);
                }
            }
            let printer_uri = format!("ipp://{}/ipp/print", host_ipp);
            let resp_bytes = handle_ipp_body(printer.as_ref(), &printer_uri, &body);
            let conn_hdr = if req.keep_alive { "keep-alive" } else { "close" };
            let resp_head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
                resp_bytes.len(),
                conn_hdr
            );
            reader
                .get_mut()
                .write_all(resp_head.as_bytes())
                .map_err(|e| format!("write mock head: {}", e))?;
            reader
                .get_mut()
                .write_all(&resp_bytes)
                .map_err(|e| format!("write mock body: {}", e))?;
            reader.get_mut().flush().ok();
        }

        if !req.keep_alive {
            return Ok(());
        }
    }
}

/// Walk a binary IPP/2.0 response (RFC 8010) and fix any `TAG_URI (0x45)` values that:
/// 1. Contain Epson ET-2720's firmware bug (`http://localhost<garbage>/...`), or
/// 2. Point to `localhost` instead of the ESP32-P4 bridge's reachable hostname/IP.
fn sanitize_ipp_response(raw: &[u8], host_http: &str, host_ipp: &str) -> Vec<u8> {
    const TAG_END_OF_ATTRIBUTES: u8 = 0x03;
    const TAG_URI: u8 = 0x45;

    if raw.len() < 9 {
        return raw.to_vec();
    }

    let mut out = Vec::with_capacity(raw.len() + 256);
    // Copy 8-byte IPP header (version + status-code + request-id)
    out.extend_from_slice(&raw[..8]);
    let mut pos = 8;

    while pos < raw.len() {
        let tag = raw[pos];
        out.push(tag);
        pos += 1;

        if tag == TAG_END_OF_ATTRIBUTES {
            // Copy any remaining payload bytes untouched
            out.extend_from_slice(&raw[pos..]);
            return out;
        }
        // Delimiter tag (0x00..=0x0F): starts a new attribute group, no name/value
        if tag < 0x10 {
            continue;
        }

        if pos + 2 > raw.len() {
            out.extend_from_slice(&raw[pos..]);
            return out;
        }
        let name_len = u16::from_be_bytes([raw[pos], raw[pos + 1]]) as usize;
        if pos + 2 + name_len + 2 > raw.len() {
            out.extend_from_slice(&raw[pos..]);
            return out;
        }
        let name_bytes = &raw[pos + 2..pos + 2 + name_len];
        let val_len_pos = pos + 2 + name_len;
        let val_len = u16::from_be_bytes([raw[val_len_pos], raw[val_len_pos + 1]]) as usize;
        let val_start = val_len_pos + 2;
        if val_start + val_len > raw.len() {
            out.extend_from_slice(&raw[pos..]);
            return out;
        }
        let val_bytes = &raw[val_start..val_start + val_len];

        // Emit name_len + name
        out.extend_from_slice(&(name_len as u16).to_be_bytes());
        out.extend_from_slice(name_bytes);

        if tag == TAG_URI {
            if let Some(fixed) = fix_ipp_uri(val_bytes, host_http, host_ipp) {
                out.extend_from_slice(&(fixed.len() as u16).to_be_bytes());
                out.extend_from_slice(fixed.as_bytes());
            } else {
                out.extend_from_slice(&(val_len as u16).to_be_bytes());
                out.extend_from_slice(val_bytes);
            }
        } else {
            out.extend_from_slice(&(val_len as u16).to_be_bytes());
            out.extend_from_slice(val_bytes);
        }

        pos = val_start + val_len;
    }

    out
}

/// Fix malformed Epson `localhost` URIs:
/// - `http://localhost\xXX/PRESENTATION/BONJOUR` -> `http://<host_http>/PRESENTATION/BONJOUR`
/// - `http://localhost /LANGUAGES/IPP?LANG=en`   -> `http://<host_http>/LANGUAGES/IPP?LANG=en`
/// - `http://localhost/icon.png`                 -> `http://<host_http>/icon.png`
/// - `ipp://localhost/ipp/print`                 -> `ipp://<host_ipp>/ipp/print`
fn fix_ipp_uri(val: &[u8], host_http: &str, host_ipp: &str) -> Option<String> {
    if val.starts_with(b"http://localhost") {
        let rest = &val[b"http://localhost".len()..];
        let path_start = rest.iter().position(|&b| b == b'/')?;
        let path = String::from_utf8_lossy(&rest[path_start..]);
        return Some(format!("http://{}{}", host_http, path));
    }
    if val.starts_with(b"https://localhost") {
        let rest = &val[b"https://localhost".len()..];
        let path_start = rest.iter().position(|&b| b == b'/')?;
        let path = String::from_utf8_lossy(&rest[path_start..]);
        return Some(format!("http://{}{}", host_http, path));
    }
    if val.starts_with(b"ipp://localhost") || val.starts_with(b"ipps://localhost") {
        let prefix_len = if val.starts_with(b"ipps://localhost") {
            b"ipps://localhost".len()
        } else {
            b"ipp://localhost".len()
        };
        let rest = &val[prefix_len..];
        let path_start = rest.iter().position(|&b| b == b'/')?;
        let path = String::from_utf8_lossy(&rest[path_start..]);
        return Some(format!("ipp://{}{}", host_ipp, path));
    }
    None
}
