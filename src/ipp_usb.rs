use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use esp_idf_svc::sys;

/// USB Bulk buffer size (must be an exact multiple of High-Speed Bulk MPS = 512 bytes).
const BULK_BUF_SIZE: usize = 16 * 1024;
/// Maximum timeout waiting for a single USB Bulk IN chunk (flatbed scan warm-up can take ~25s).
const BULK_IN_TIMEOUT: Duration = Duration::from_secs(45);
const BULK_OUT_TIMEOUT: Duration = Duration::from_secs(15);
const CTRL_TIMEOUT: Duration = Duration::from_secs(5);

struct TransferWaiter {
    result: Mutex<Option<(sys::usb_transfer_status_t, usize)>>,
    cvar: Condvar,
}

impl TransferWaiter {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            cvar: Condvar::new(),
        }
    }

    fn reset(&self) {
        *self.result.lock().unwrap() = None;
    }

    fn complete(&self, status: sys::usb_transfer_status_t, actual_bytes: usize) {
        let mut guard = self.result.lock().unwrap();
        *guard = Some((status, actual_bytes));
        self.cvar.notify_one();
    }

    fn wait(&self, timeout: Duration) -> Result<(sys::usb_transfer_status_t, usize), String> {
        let guard = self.result.lock().unwrap();
        let (guard, timeout_res) = self
            .cvar
            .wait_timeout_while(guard, timeout, |r| r.is_none())
            .unwrap();
        if timeout_res.timed_out() {
            return Err("USB transfer timed out".to_string());
        }
        Ok(guard.unwrap())
    }
}

unsafe extern "C" fn usb_transfer_cb(transfer: *mut sys::usb_transfer_t) {
    if transfer.is_null() {
        return;
    }
    let ctx = (*transfer).context;
    if ctx.is_null() {
        return;
    }
    let waiter = &*(ctx as *const TransferWaiter);
    waiter.complete((*transfer).status, (*transfer).actual_num_bytes as usize);
}

/// Send USB Standard SET_INTERFACE control transfer (bmRequestType=0x01, bRequest=0x0B)
/// so the printer switches the interface into its IPP-USB alternate setting (alt 1).
pub unsafe fn send_set_interface(
    client_hdl: sys::usb_host_client_handle_t,
    dev_hdl: sys::usb_device_handle_t,
    iface_num: u8,
    alt_setting: u8,
) -> Result<(), String> {
    let mut xfer: *mut sys::usb_transfer_t = ptr::null_mut();
    let err = sys::usb_host_transfer_alloc(8, 0, &mut xfer);
    if err != sys::ESP_OK || xfer.is_null() {
        return Err(format!("usb_host_transfer_alloc(ctrl) failed: {}", err));
    }

    let waiter = Box::new(TransferWaiter::new());
    let waiter_ptr = &*waiter as *const TransferWaiter as *mut core::ffi::c_void;

    // 8-byte USB Setup packet for SET_INTERFACE
    let setup = [
        0x01u8, // bmRequestType: Host-to-Device | Standard | Interface
        0x0Bu8, // bRequest: SET_INTERFACE
        alt_setting,
        0x00, // wValue (alt setting LE)
        iface_num,
        0x00, // wIndex (interface number LE)
        0x00,
        0x00, // wLength = 0
    ];
    ptr::copy_nonoverlapping(setup.as_ptr(), (*xfer).data_buffer, 8);
    (*xfer).num_bytes = 8;
    (*xfer).device_handle = dev_hdl;
    (*xfer).bEndpointAddress = 0x00;
    (*xfer).callback = Some(usb_transfer_cb);
    (*xfer).context = waiter_ptr;

    let submit_err = sys::usb_host_transfer_submit_control(client_hdl, xfer);
    if submit_err != sys::ESP_OK {
        let _ = sys::usb_host_transfer_free(xfer);
        return Err(format!("SET_INTERFACE submit failed: {}", submit_err));
    }

    let res = waiter.wait(CTRL_TIMEOUT);
    let _ = sys::usb_host_transfer_free(xfer);

    match res {
        Ok((sys::usb_transfer_status_t_USB_TRANSFER_STATUS_COMPLETED, _)) => Ok(()),
        Ok((st, _)) => Err(format!("SET_INTERFACE status={}", st)),
        Err(e) => Err(e),
    }
}

/// One claimed IPP-USB interface channel (Bulk OUT + Bulk IN pair).
pub struct IppUsbChannel {
    pub iface_num: u8,
    pub alt_setting: u8,
    pub ep_out: u8,
    pub ep_in: u8,
    pub mps_in: u16,
    dev_hdl: sys::usb_device_handle_t,
    xfer_out: *mut sys::usb_transfer_t,
    xfer_in: *mut sys::usb_transfer_t,
    waiter_out: Box<TransferWaiter>,
    waiter_in: Box<TransferWaiter>,
    /// Leftover bytes read from USB Bulk IN that belong to the current HTTP stream.
    rx_buf: Vec<u8>,
}

unsafe impl Send for IppUsbChannel {}

impl IppUsbChannel {
    pub unsafe fn new(
        dev_hdl: sys::usb_device_handle_t,
        iface_num: u8,
        alt_setting: u8,
        ep_out: u8,
        ep_in: u8,
        mps_in: u16,
    ) -> Result<Self, String> {
        let mut xfer_out: *mut sys::usb_transfer_t = ptr::null_mut();
        let mut xfer_in: *mut sys::usb_transfer_t = ptr::null_mut();

        let err1 = sys::usb_host_transfer_alloc(BULK_BUF_SIZE, 0, &mut xfer_out);
        if err1 != sys::ESP_OK || xfer_out.is_null() {
            return Err(format!("alloc xfer_out failed: {}", err1));
        }
        let err2 = sys::usb_host_transfer_alloc(BULK_BUF_SIZE, 0, &mut xfer_in);
        if err2 != sys::ESP_OK || xfer_in.is_null() {
            let _ = sys::usb_host_transfer_free(xfer_out);
            return Err(format!("alloc xfer_in failed: {}", err2));
        }

        let waiter_out = Box::new(TransferWaiter::new());
        let waiter_in = Box::new(TransferWaiter::new());

        (*xfer_out).device_handle = dev_hdl;
        (*xfer_out).bEndpointAddress = ep_out;
        (*xfer_out).callback = Some(usb_transfer_cb);
        (*xfer_out).context = &*waiter_out as *const TransferWaiter as *mut core::ffi::c_void;

        (*xfer_in).device_handle = dev_hdl;
        (*xfer_in).bEndpointAddress = ep_in;
        (*xfer_in).callback = Some(usb_transfer_cb);
        (*xfer_in).context = &*waiter_in as *const TransferWaiter as *mut core::ffi::c_void;

        Ok(Self {
            iface_num,
            alt_setting,
            ep_out,
            ep_in,
            mps_in: mps_in.max(64),
            dev_hdl,
            xfer_out,
            xfer_in,
            waiter_out,
            waiter_in,
            rx_buf: Vec::with_capacity(4096),
        })
    }

    /// Write all bytes to the Bulk OUT endpoint in <=16 KB chunks.
    pub fn bulk_write_all(&mut self, data: &[u8]) -> Result<(), String> {
        for chunk in data.chunks(BULK_BUF_SIZE) {
            unsafe {
                ptr::copy_nonoverlapping(chunk.as_ptr(), (*self.xfer_out).data_buffer, chunk.len());
                (*self.xfer_out).num_bytes = chunk.len() as i32;
                (*self.xfer_out).flags = 0;
                self.waiter_out.reset();

                let err = sys::usb_host_transfer_submit(self.xfer_out);
                if err != sys::ESP_OK {
                    return Err(format!("Bulk OUT (0x{:02X}) submit err={}", self.ep_out, err));
                }

                match self.waiter_out.wait(BULK_OUT_TIMEOUT) {
                    Ok((sys::usb_transfer_status_t_USB_TRANSFER_STATUS_COMPLETED, _)) => {}
                    Ok((st, _)) => {
                        self.recover_endpoint(self.ep_out);
                        return Err(format!("Bulk OUT (0x{:02X}) status={}", self.ep_out, st));
                    }
                    Err(e) => {
                        self.recover_endpoint(self.ep_out);
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }

    /// Read one Bulk IN transfer from USB and append into `self.rx_buf`.
    fn bulk_read_more(&mut self, timeout: Duration) -> Result<usize, String> {
        unsafe {
            // Round transfer size down to exact multiple of MPS (512)
            let mps = self.mps_in as usize;
            let req_len = (BULK_BUF_SIZE / mps) * mps;
            (*self.xfer_in).num_bytes = req_len as i32;
            (*self.xfer_in).flags = 0;
            self.waiter_in.reset();

            let err = sys::usb_host_transfer_submit(self.xfer_in);
            if err != sys::ESP_OK {
                return Err(format!("Bulk IN (0x{:02X}) submit err={}", self.ep_in, err));
            }

            match self.waiter_in.wait(timeout) {
                Ok((sys::usb_transfer_status_t_USB_TRANSFER_STATUS_COMPLETED, n)) => {
                    if n > 0 {
                        let slice = core::slice::from_raw_parts((*self.xfer_in).data_buffer, n);
                        self.rx_buf.extend_from_slice(slice);
                    }
                    Ok(n)
                }
                Ok((st, _)) => {
                    self.recover_endpoint(self.ep_in);
                    Err(format!("Bulk IN (0x{:02X}) status={}", self.ep_in, st))
                }
                Err(e) => {
                    self.recover_endpoint(self.ep_in);
                    Err(e)
                }
            }
        }
    }

    unsafe fn recover_endpoint(&self, ep: u8) {
        let _ = sys::usb_host_endpoint_halt(self.dev_hdl, ep);
        let _ = sys::usb_host_endpoint_flush(self.dev_hdl, ep);
        let _ = sys::usb_host_endpoint_clear(self.dev_hdl, ep);
    }

    /// Best-effort drain of any unread HTTP response bytes after a client cancel/abort,
    /// so the persistent IPP-USB channel never feeds stale bytes into the next request.
    fn drain_stale_rx(&mut self) {
        self.rx_buf.clear();
        for _ in 0..16 {
            match self.bulk_read_more(Duration::from_millis(250)) {
                Ok(0) | Err(_) => break,
                Ok(_) => self.rx_buf.clear(),
            }
        }
        self.rx_buf.clear();
    }
}

impl Drop for IppUsbChannel {
    fn drop(&mut self) {
        unsafe {
            if !self.xfer_out.is_null() {
                let _ = sys::usb_host_transfer_free(self.xfer_out);
            }
            if !self.xfer_in.is_null() {
                let _ = sys::usb_host_transfer_free(self.xfer_in);
            }
        }
    }
}

/// Parsed HTTP/1.1 response header from the printer over IPP-USB.
#[derive(Debug, Clone)]
pub struct HttpResponseHead {
    pub status_code: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub content_length: Option<usize>,
    pub chunked: bool,
}

impl HttpResponseHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Thread-safe pool of IPP-USB channels for the currently connected USB printer.
pub struct IppUsbBridge {
    pool: Mutex<Vec<IppUsbChannel>>,
    cvar: Condvar,
    /// Atomic flag (never nested with `pool` lock) to prevent any lock-order deadlock on disconnect.
    connected: AtomicBool,
    product_name: Mutex<String>,
}

impl IppUsbBridge {
    pub fn new() -> Self {
        Self {
            pool: Mutex::new(Vec::new()),
            cvar: Condvar::new(),
            connected: AtomicBool::new(false),
            product_name: Mutex::new(String::new()),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn product_name(&self) -> String {
        self.product_name.lock().unwrap().clone()
    }

    pub fn set_channels(&self, product: String, channels: Vec<IppUsbChannel>) {
        let count = channels.len();
        *self.product_name.lock().unwrap() = product;
        {
            let mut pool = self.pool.lock().unwrap();
            *pool = channels;
            self.connected.store(count > 0, Ordering::SeqCst);
        }
        self.cvar.notify_all();
    }

    pub fn clear(&self) {
        self.connected.store(false, Ordering::SeqCst);
        {
            let mut pool = self.pool.lock().unwrap();
            pool.clear();
        }
        self.cvar.notify_all();
    }

    /// Borrow an idle IPP-USB channel from the pool.
    fn acquire_channel(&self, timeout: Duration) -> Result<IppUsbChannel, String> {
        let guard = self.pool.lock().unwrap();
        let (mut guard, res) = self
            .cvar
            .wait_timeout_while(guard, timeout, |p| {
                p.is_empty() && self.connected.load(Ordering::SeqCst)
            })
            .unwrap();
        if res.timed_out() {
            return Err("All IPP-USB channels busy (timed out)".to_string());
        }
        guard
            .pop()
            .ok_or_else(|| "IPP-USB printer not connected".to_string())
    }

    fn release_channel(&self, mut ch: IppUsbChannel, was_err: bool) {
        if was_err && self.connected.load(Ordering::SeqCst) {
            log::warn!(
                "[IPP-USB IF#{}] Cleaning channel after aborted/errored transfer",
                ch.iface_num
            );
            ch.drain_stale_rx();
        } else {
            ch.rx_buf.clear();
        }
        if self.connected.load(Ordering::SeqCst) {
            let mut pool = self.pool.lock().unwrap();
            pool.push(ch);
            self.cvar.notify_one();
        }
    }

    /// Execute an HTTP/1.1 request over IPP-USB, calling `on_head` once headers arrive
    /// (which produces a writer state `W`) and `on_body_chunk(&mut w, chunk)` as body bytes arrive.
    pub fn exchange_streaming<W, FH, FB>(
        &self,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        req_body: &[u8],
        on_head: FH,
        mut on_body_chunk: FB,
    ) -> Result<W, String>
    where
        FH: FnOnce(&HttpResponseHead) -> Result<W, String>,
        FB: FnMut(&mut W, &[u8]) -> Result<(), String>,
    {
        let mut ch = self.acquire_channel(Duration::from_secs(30))?;
        let result = (|| {
            ch.rx_buf.clear();

            let mut req = format!("{method} {uri} HTTP/1.1\r\nHost: localhost\r\n");
            if let Some(ct) = content_type {
                req.push_str(&format!("Content-Type: {ct}\r\n"));
            }
            if !req_body.is_empty() || method == "POST" || method == "PUT" {
                req.push_str(&format!("Content-Length: {}\r\n", req_body.len()));
            }
            req.push_str("Connection: keep-alive\r\n\r\n");

            let mut wire = req.into_bytes();
            wire.extend_from_slice(req_body);
            ch.bulk_write_all(&wire)?;

            Self::read_response_from_channel(&mut ch, on_head, &mut on_body_chunk)
        })();
        self.release_channel(ch, result.is_err());
        result
    }

    /// Execute an HTTP/1.1 request where BOTH request and response bodies are streamed.
    pub fn exchange_stream_both<R, W, FR, FH, FB>(
        &self,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        content_length: Option<usize>,
        mut reader: R,
        mut read_req_chunk: FR,
        on_head: FH,
        mut on_body_chunk: FB,
    ) -> Result<W, String>
    where
        FR: FnMut(&mut R, &mut [u8]) -> Result<usize, String>,
        FH: FnOnce(R, &HttpResponseHead) -> Result<W, String>,
        FB: FnMut(&mut W, &[u8]) -> Result<(), String>,
    {
        let mut ch = self.acquire_channel(Duration::from_secs(30))?;
        let result = (|| {
            ch.rx_buf.clear();

            let mut hdr = format!("{method} {uri} HTTP/1.1\r\nHost: localhost\r\n");
            if let Some(ct) = content_type {
                hdr.push_str(&format!("Content-Type: {ct}\r\n"));
            }
            let use_chunked_req = content_length.is_none();
            if let Some(len) = content_length {
                hdr.push_str(&format!("Content-Length: {len}\r\n"));
            } else {
                hdr.push_str("Transfer-Encoding: chunked\r\n");
            }
            hdr.push_str("Connection: keep-alive\r\n\r\n");
            ch.bulk_write_all(hdr.as_bytes())?;

            let mut buf = vec![0u8; 8192];
            let mut total_sent = 0usize;
            loop {
                let n = read_req_chunk(&mut reader, &mut buf)?;
                if n == 0 {
                    break;
                }
                total_sent += n;
                if use_chunked_req {
                    let prefix = format!("{:x}\r\n", n);
                    ch.bulk_write_all(prefix.as_bytes())?;
                    ch.bulk_write_all(&buf[..n])?;
                    ch.bulk_write_all(b"\r\n")?;
                } else {
                    ch.bulk_write_all(&buf[..n])?;
                }
            }
            if use_chunked_req {
                ch.bulk_write_all(b"0\r\n\r\n")?;
            }
            log::debug!(
                "[IPP-USB IF#{}] Sent {} {} ({} body bytes)",
                ch.iface_num,
                method,
                uri,
                total_sent
            );

            Self::read_response_from_channel(&mut ch, |h| on_head(reader, h), &mut on_body_chunk)
        })();

        self.release_channel(ch, result.is_err());
        result
    }

    /// Convenience helper for small requests (e.g., probing ScannerCapabilities).
    pub fn request_buffered(
        &self,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        req_body: &[u8],
    ) -> Result<(HttpResponseHead, Vec<u8>), String> {
        self.exchange_streaming(
            method,
            uri,
            content_type,
            req_body,
            |h| Ok((h.clone(), Vec::new())),
            |(_, body), chunk| {
                body.extend_from_slice(chunk);
                Ok(())
            },
        )
    }

    fn read_response_from_channel<W, FH, FB>(
        ch: &mut IppUsbChannel,
        on_head: FH,
        on_body_chunk: &mut FB,
    ) -> Result<W, String>
    where
        FH: FnOnce(&HttpResponseHead) -> Result<W, String>,
        FB: FnMut(&mut W, &[u8]) -> Result<(), String>,
    {
        // Loop to skip any interim 100 Continue responses
        let head = loop {
            let hdr_end = loop {
                if let Some(pos) = find_crlfcrlf(&ch.rx_buf) {
                    break pos;
                }
                if ch.rx_buf.len() > 32 * 1024 {
                    return Err("HTTP response headers exceeded 32KB".to_string());
                }
                let n = ch.bulk_read_more(BULK_IN_TIMEOUT)?;
                if n == 0 && ch.rx_buf.is_empty() {
                    continue;
                }
            };

            let hdr_bytes = ch.rx_buf[..hdr_end].to_vec();
            ch.rx_buf.drain(..hdr_end + 4);

            let parsed = parse_http_head(&hdr_bytes)?;
            if parsed.status_code == 100 {
                log::debug!("[IPP-USB] Skipping interim 100 Continue");
                continue;
            }
            break parsed;
        };

        let mut writer = on_head(&head)?;

        if head.status_code == 204 || head.status_code == 304 || head.content_length == Some(0) {
            return Ok(writer);
        }

        if head.chunked {
            Self::stream_chunked_body(ch, &mut writer, on_body_chunk)?;
        } else if let Some(mut remaining) = head.content_length {
            while remaining > 0 {
                if ch.rx_buf.is_empty() {
                    let n = ch.bulk_read_more(BULK_IN_TIMEOUT)?;
                    if n == 0 {
                        continue;
                    }
                }
                let take = remaining.min(ch.rx_buf.len());
                on_body_chunk(&mut writer, &ch.rx_buf[..take])?;
                ch.rx_buf.drain(..take);
                remaining -= take;
            }
        } else if !ch.rx_buf.is_empty() {
            let data = std::mem::take(&mut ch.rx_buf);
            on_body_chunk(&mut writer, &data)?;
        }

        Ok(writer)
    }

    /// Decode HTTP/1.1 Transfer-Encoding: chunked from USB Bulk IN cleanly until `0\r\n\r\n`.
    fn stream_chunked_body<W, FB>(
        ch: &mut IppUsbChannel,
        writer: &mut W,
        on_body_chunk: &mut FB,
    ) -> Result<(), String>
    where
        FB: FnMut(&mut W, &[u8]) -> Result<(), String>,
    {
        loop {
            let line_end = loop {
                if let Some(pos) = find_crlf(&ch.rx_buf) {
                    break pos;
                }
                let _ = ch.bulk_read_more(BULK_IN_TIMEOUT)?;
            };

            let size_line = String::from_utf8_lossy(&ch.rx_buf[..line_end]).into_owned();
            ch.rx_buf.drain(..line_end + 2);

            let size_hex = size_line.split(';').next().unwrap_or("").trim();
            let chunk_size = usize::from_str_radix(size_hex, 16)
                .map_err(|_| format!("Invalid HTTP chunk size: {:?}", size_line))?;

            if chunk_size == 0 {
                loop {
                    if ch.rx_buf.starts_with(b"\r\n") {
                        ch.rx_buf.drain(..2);
                        break;
                    }
                    if let Some(pos) = find_crlfcrlf(&ch.rx_buf) {
                        ch.rx_buf.drain(..pos + 4);
                        break;
                    }
                    let _ = ch.bulk_read_more(Duration::from_secs(3))?;
                }
                return Ok(());
            }

            let mut rem = chunk_size;
            while rem > 0 {
                if ch.rx_buf.is_empty() {
                    let _ = ch.bulk_read_more(BULK_IN_TIMEOUT)?;
                }
                let take = rem.min(ch.rx_buf.len());
                if take > 0 {
                    on_body_chunk(writer, &ch.rx_buf[..take])?;
                    ch.rx_buf.drain(..take);
                    rem -= take;
                }
            }

            while ch.rx_buf.len() < 2 {
                let _ = ch.bulk_read_more(BULK_IN_TIMEOUT)?;
            }
            if ch.rx_buf.starts_with(b"\r\n") {
                ch.rx_buf.drain(..2);
            }
        }
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

fn find_crlfcrlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_http_head(raw: &[u8]) -> Result<HttpResponseHead, String> {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or("Empty HTTP status line")?;
    let mut parts = status_line.splitn(3, ' ');
    let _http_ver = parts.next().ok_or("Missing HTTP version")?;
    let status_code: u16 = parts
        .next()
        .ok_or("Missing HTTP status code")?
        .parse()
        .map_err(|_| format!("Bad HTTP status line: {}", status_line))?;
    let status_text = parts.next().unwrap_or("OK").to_string();

    let mut headers = Vec::new();
    let mut content_length = None;
    let mut chunked = false;

    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let val = v.trim().to_string();
            if key.eq_ignore_ascii_case("Content-Length") {
                content_length = val.parse::<usize>().ok();
            } else if key.eq_ignore_ascii_case("Transfer-Encoding")
                && val.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
            headers.push((key, val));
        }
    }

    Ok(HttpResponseHead {
        status_code,
        status_text,
        headers,
        content_length,
        chunked,
    })
}

/// Rewrite localhost URLs returned by the printer over IPP-USB (e.g. `Location: http://localhost/eSCL/ScanJobs/1`)
/// so network clients like macOS Image Capture follow the ESP32-P4's address instead.
pub fn rewrite_localhost_url(val: &str, p4_host: &str) -> String {
    for prefix in [
        "http://localhost:80",
        "http://localhost:631",
        "http://localhost",
        "https://localhost:443",
        "https://localhost",
        "http://127.0.0.1",
    ] {
        if let Some(rest) = val.strip_prefix(prefix) {
            return format!("http://{}{}", p4_host, rest);
        }
    }
    val.to_string()
}
