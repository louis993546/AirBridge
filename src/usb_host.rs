use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;

use esp_idf_svc::sys::{self, esp, EspError};

use crate::ipp_usb::{send_set_interface, IppUsbBridge, IppUsbChannel};

#[derive(Debug, Clone)]
pub struct UsbEndpointInfo {
    pub address: u8,
    pub is_in: bool,
    pub transfer_type: &'static str,
    pub max_packet_size: u16,
}

#[derive(Debug, Clone)]
pub struct UsbInterfaceInfo {
    pub number: u8,
    pub alt_setting: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub label: &'static str,
    pub endpoints: Vec<UsbEndpointInfo>,
}

#[derive(Debug, Clone)]
pub struct UsbDeviceInfo {
    pub address: u8,
    pub vid: u16,
    pub pid: u16,
    pub bcd_usb: u16,
    pub speed: &'static str,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
    pub interfaces: Vec<UsbInterfaceInfo>,
    pub has_ipp_usb: bool,
    pub has_printer_class: bool,
    pub has_vendor_scanner: bool,
}

#[derive(Debug, Default)]
pub struct UsbHostState {
    pub host_installed: bool,
    pub devices: Vec<UsbDeviceInfo>,
    pub ipp_usb_channels: usize,
    pub last_event: String,
}

impl UsbHostState {
    pub fn summary_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "USB Host installed: {}\nIPP-USB active channels: {}\nLast event: {}\nConnected devices: {}\n",
            self.host_installed,
            self.ipp_usb_channels,
            self.last_event,
            self.devices.len()
        ));
        for dev in &self.devices {
            out.push_str(&format!(
                "\n[Addr {}] {:04X}:{:04X} | {} | {} | S/N: {} | Speed: {} | USB {:x}.{:02x}\n",
                dev.address,
                dev.vid,
                dev.pid,
                dev.manufacturer,
                dev.product,
                dev.serial,
                dev.speed,
                dev.bcd_usb >> 8,
                dev.bcd_usb & 0xFF
            ));
            out.push_str(&format!(
                "  Capabilities -> IPP-USB: {}, PrinterClass: {}, VendorScanner: {}\n",
                dev.has_ipp_usb, dev.has_printer_class, dev.has_vendor_scanner
            ));
            for iface in &dev.interfaces {
                out.push_str(&format!(
                    "  IF #{} (alt {}): class={:02X}/{:02X}/{:02X} [{}]\n",
                    iface.number,
                    iface.alt_setting,
                    iface.class,
                    iface.subclass,
                    iface.protocol,
                    iface.label
                ));
                for ep in &iface.endpoints {
                    out.push_str(&format!(
                        "    EP 0x{:02X} ({}, {}, MPS={})\n",
                        ep.address,
                        if ep.is_in { "IN" } else { "OUT" },
                        ep.transfer_type,
                        ep.max_packet_size
                    ));
                }
            }
        }
        out
    }
}

struct OpenedDevice {
    addr: u8,
    dev_hdl: sys::usb_device_handle_t,
    claimed_ifaces: Vec<u8>,
}

unsafe impl Send for OpenedDevice {}

struct ClientCtx {
    client_hdl: sys::usb_host_client_handle_t,
    state: Arc<Mutex<UsbHostState>>,
    ipp_usb: Arc<IppUsbBridge>,
    opened: Arc<Mutex<Vec<OpenedDevice>>>,
}

/// Start ESP-IDF High-Speed USB Host stack (with CH334F Hub support), enumerate connected devices,
/// and automatically claim IPP-USB (07/01/04) interfaces for AirScan & AirPrint proxying.
pub fn start_usb_host(ipp_usb: Arc<IppUsbBridge>) -> Result<Arc<Mutex<UsbHostState>>, EspError> {
    let state = Arc::new(Mutex::new(UsbHostState {
        host_installed: false,
        devices: Vec::new(),
        ipp_usb_channels: 0,
        last_event: "Initializing USB Host...".to_string(),
    }));

    unsafe {
        let mut cfg: sys::usb_host_config_t = core::mem::zeroed();
        cfg.skip_phy_setup = false;
        cfg.root_port_unpowered = false;
        cfg.intr_flags = sys::ESP_INTR_FLAG_LEVEL1 as i32;
        cfg.peripheral_map = 0; // High-Speed USB 2.0 OTG peripheral on ESP32-P4

        esp!(sys::usb_host_install(&cfg))?;
    }

    {
        let mut s = state.lock().unwrap();
        s.host_installed = true;
        s.last_event =
            "USB Host installed (High-Speed OTG + Hub support), waiting for devices...".to_string();
    }
    log::info!("[USB] High-Speed USB Host stack installed (CH334F hub enabled)");

    // Thread 1: USB Host Library daemon event handler
    thread::Builder::new()
        .name("usb_host_lib".into())
        .stack_size(8192)
        .spawn(|| {
            log::info!("[USB] Library daemon thread started");
            loop {
                let mut event_flags: u32 = 0;
                let err = unsafe {
                    sys::usb_host_lib_handle_events(sys::TickType_t::MAX, &mut event_flags)
                };
                if err != sys::ESP_OK && err != sys::ESP_ERR_TIMEOUT {
                    log::warn!("[USB] usb_host_lib_handle_events err={}", err);
                }
            }
        })
        .expect("spawn usb_host_lib thread");

    // Thread 2: USB Host Client event loop (enumerates devices & pumps transfer completion callbacks)
    let client_state = state.clone();
    thread::Builder::new()
        .name("usb_host_client".into())
        .stack_size(12288)
        .spawn(move || {
            let ctx = Box::new(ClientCtx {
                client_hdl: ptr::null_mut(),
                state: client_state,
                ipp_usb,
                opened: Arc::new(Mutex::new(Vec::new())),
            });
            let ctx_ptr = Box::into_raw(ctx);

            let mut client_cfg: sys::usb_host_client_config_t = unsafe { core::mem::zeroed() };
            client_cfg.is_synchronous = false;
            client_cfg.max_num_event_msg = 16;
            client_cfg.__bindgen_anon_1.async_ =
                sys::usb_host_client_config_t__bindgen_ty_1__bindgen_ty_1 {
                    client_event_callback: Some(client_event_cb),
                    callback_arg: ctx_ptr as *mut core::ffi::c_void,
                };

            let mut client_hdl: sys::usb_host_client_handle_t = ptr::null_mut();
            let err = unsafe { sys::usb_host_client_register(&client_cfg, &mut client_hdl) };
            if err != sys::ESP_OK {
                log::error!("[USB] usb_host_client_register failed: {}", err);
                return;
            }
            unsafe {
                (*ctx_ptr).client_hdl = client_hdl;

                // Inspect any device that enumerated before our client registered
                let mut addrs = [0u8; 8];
                let mut num_dev: i32 = 0;
                if sys::usb_host_device_addr_list_fill(
                    addrs.len() as i32,
                    addrs.as_mut_ptr(),
                    &mut num_dev,
                ) == sys::ESP_OK
                {
                    log::info!("[USB] Pre-existing enumerated devices: {}", num_dev);
                    for &addr in &addrs[..num_dev.max(0) as usize] {
                        inspect_and_record_device(&*ctx_ptr, addr);
                    }
                }
            }
            log::info!("[USB] Client registered; watching for USB devices...");

            loop {
                let err = unsafe {
                    sys::usb_host_client_handle_events(client_hdl, sys::TickType_t::MAX)
                };
                if err != sys::ESP_OK && err != sys::ESP_ERR_TIMEOUT {
                    log::warn!("[USB] usb_host_client_handle_events err={}", err);
                }
            }
        })
        .expect("spawn usb_host_client thread");

    Ok(state)
}

unsafe extern "C" fn client_event_cb(
    event_msg: *const sys::usb_host_client_event_msg_t,
    arg: *mut core::ffi::c_void,
) {
    let ctx = &*(arg as *const ClientCtx);
    let msg = &*event_msg;

    match msg.event {
        sys::usb_host_client_event_t_USB_HOST_CLIENT_EVENT_NEW_DEV => {
            let addr = msg.__bindgen_anon_1.new_dev.address;
            log::info!("[USB] >>> NEW DEVICE detected at address {}", addr);
            inspect_and_record_device(ctx, addr);
        }
        sys::usb_host_client_event_t_USB_HOST_CLIENT_EVENT_DEV_GONE => {
            let dev_hdl = msg.__bindgen_anon_1.dev_gone.dev_hdl;
            log::warn!("[USB] <<< DEVICE GONE (handle={:?})", dev_hdl);

            // Drop all active IPP-USB channels before releasing interfaces & closing device
            ctx.ipp_usb.clear();

            let mut removed = None;
            if let Ok(mut opened) = ctx.opened.lock() {
                if let Some(pos) = opened.iter().position(|d| d.dev_hdl == dev_hdl) {
                    removed = Some(opened.remove(pos));
                }
            }
            if let Some(dev) = removed {
                for iface_num in dev.claimed_ifaces {
                    let _ = sys::usb_host_interface_release(ctx.client_hdl, dev_hdl, iface_num);
                }
                let _ = sys::usb_host_device_close(ctx.client_hdl, dev_hdl);
                if let Ok(mut st) = ctx.state.lock() {
                    st.devices.retain(|d| d.address != dev.addr);
                    st.ipp_usb_channels = 0;
                    st.last_event = format!("Device at addr {} disconnected", dev.addr);
                }
            } else {
                let _ = sys::usb_host_device_close(ctx.client_hdl, dev_hdl);
            }
        }
        _ => {}
    }
}

unsafe fn inspect_and_record_device(ctx: &ClientCtx, addr: u8) {
    let mut dev_hdl: sys::usb_device_handle_t = ptr::null_mut();
    let err = sys::usb_host_device_open(ctx.client_hdl, addr, &mut dev_hdl);
    if err != sys::ESP_OK {
        log::error!("[USB] Failed to open device addr {}: err={}", addr, err);
        if let Ok(mut st) = ctx.state.lock() {
            st.last_event = format!("Failed to open USB addr {} (err={})", addr, err);
        }
        return;
    }

    if let Ok(mut opened) = ctx.opened.lock() {
        opened.push(OpenedDevice {
            addr,
            dev_hdl,
            claimed_ifaces: Vec::new(),
        });
    }

    let mut dev_info: sys::usb_device_info_t = core::mem::zeroed();
    let _ = sys::usb_host_device_info(dev_hdl, &mut dev_info);

    let speed = match dev_info.speed {
        sys::usb_speed_t_USB_SPEED_LOW => "Low-Speed (1.5 Mbps)",
        sys::usb_speed_t_USB_SPEED_FULL => "Full-Speed (12 Mbps)",
        sys::usb_speed_t_USB_SPEED_HIGH => "High-Speed (480 Mbps)",
        _ => "Unknown",
    };

    let manufacturer = decode_usb_string(dev_info.str_desc_manufacturer);
    let product = decode_usb_string(dev_info.str_desc_product);
    let serial = decode_usb_string(dev_info.str_desc_serial_num);

    let mut dev_desc_ptr: *const sys::usb_device_desc_t = ptr::null();
    let (vid, pid, bcd_usb) =
        if sys::usb_host_get_device_descriptor(dev_hdl, &mut dev_desc_ptr) == sys::ESP_OK
            && !dev_desc_ptr.is_null()
        {
            let b = &(*dev_desc_ptr).val;
            let bcd = u16::from_le_bytes([b[2], b[3]]);
            let vid = u16::from_le_bytes([b[8], b[9]]);
            let pid = u16::from_le_bytes([b[10], b[11]]);
            (vid, pid, bcd)
        } else {
            (0, 0, 0)
        };

    let mut cfg_desc_ptr: *const sys::usb_config_desc_t = ptr::null();
    let mut interfaces = Vec::new();
    if sys::usb_host_get_active_config_descriptor(dev_hdl, &mut cfg_desc_ptr) == sys::ESP_OK
        && !cfg_desc_ptr.is_null()
    {
        let hdr = &(*cfg_desc_ptr).val;
        let total_len = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
        let raw = core::slice::from_raw_parts(cfg_desc_ptr as *const u8, total_len);
        interfaces = parse_config_descriptor(raw);
    }

    let ipp_usb_ifaces: Vec<UsbInterfaceInfo> = interfaces
        .iter()
        .filter(|i| i.class == 0x07 && i.subclass == 0x01 && i.protocol == 0x04)
        .cloned()
        .collect();
    let has_ipp_usb = !ipp_usb_ifaces.is_empty();
    let has_printer_class = interfaces
        .iter()
        .any(|i| i.class == 0x07 && i.subclass == 0x01 && i.protocol != 0x04);
    let has_vendor_scanner = interfaces.iter().any(|i| i.class == 0xFF);

    let product_label = format!("{} {}", manufacturer, product);
    let info = UsbDeviceInfo {
        address: addr,
        vid,
        pid,
        bcd_usb,
        speed,
        manufacturer,
        product,
        serial,
        interfaces,
        has_ipp_usb,
        has_printer_class,
        has_vendor_scanner,
    };

    log::info!("========================================");
    log::info!(
        "[USB] Enumerated addr={}: {:04X}:{:04X} ({}, {})",
        info.address,
        info.vid,
        info.pid,
        product_label,
        info.speed
    );
    log::info!(
        "  IPP-USB interfaces found: {}",
        ipp_usb_ifaces.len()
    );
    log::info!("========================================");

    if let Ok(mut st) = ctx.state.lock() {
        st.devices.retain(|d| d.address != addr);
        st.last_event = format!(
            "Connected: {} ({:04X}:{:04X})",
            product_label, info.vid, info.pid
        );
        st.devices.push(info);
    }

    // Activate IPP-USB interfaces on a separate thread so we don't block usb_host_client_handle_events
    // (SET_INTERFACE control transfers need the client event loop to pump their completion callbacks!).
    if !ipp_usb_ifaces.is_empty() {
        let client_hdl_raw = ctx.client_hdl as usize;
        let dev_hdl_raw = dev_hdl as usize;
        let ipp_usb = ctx.ipp_usb.clone();
        let state = ctx.state.clone();
        let opened = ctx.opened.clone();

        let _ = thread::Builder::new()
            .name("ipp_usb_init".into())
            .stack_size(8192)
            .spawn(move || {
                let client_hdl = client_hdl_raw as sys::usb_host_client_handle_t;
                let dev_hdl = dev_hdl_raw as sys::usb_device_handle_t;
                let mut channels = Vec::new();
                let mut claimed_nums = Vec::new();

                for iface in ipp_usb_ifaces {
                    let ep_out = iface
                        .endpoints
                        .iter()
                        .find(|e| !e.is_in && e.transfer_type == "Bulk");
                    let ep_in = iface
                        .endpoints
                        .iter()
                        .find(|e| e.is_in && e.transfer_type == "Bulk");

                    let (Some(out_ep), Some(in_ep)) = (ep_out, ep_in) else {
                        continue;
                    };

                    // 1. Send USB SET_INTERFACE to switch printer into alt setting 1 (IPP-USB)
                    if let Err(e) =
                        send_set_interface(client_hdl, dev_hdl, iface.number, iface.alt_setting)
                    {
                        log::error!(
                            "[IPP-USB] SET_INTERFACE(IF #{}, alt {}) failed: {}",
                            iface.number,
                            iface.alt_setting,
                            e
                        );
                        continue;
                    }

                    // 2. Claim the interface endpoints in ESP-IDF USB Host
                    let claim_err = sys::usb_host_interface_claim(
                        client_hdl,
                        dev_hdl,
                        iface.number,
                        iface.alt_setting,
                    );
                    if claim_err != sys::ESP_OK {
                        log::error!(
                            "[IPP-USB] interface_claim(IF #{}, alt {}) err={}",
                            iface.number,
                            iface.alt_setting,
                            claim_err
                        );
                        continue;
                    }
                    claimed_nums.push(iface.number);

                    match IppUsbChannel::new(
                        dev_hdl,
                        iface.number,
                        iface.alt_setting,
                        out_ep.address,
                        in_ep.address,
                        in_ep.max_packet_size,
                    ) {
                        Ok(ch) => {
                            log::info!(
                                "[IPP-USB] Channel ready: IF #{} (alt {}), OUT=0x{:02X}, IN=0x{:02X} (MPS={})",
                                ch.iface_num,
                                ch.alt_setting,
                                ch.ep_out,
                                ch.ep_in,
                                ch.mps_in
                            );
                            channels.push(ch);
                        }
                        Err(e) => {
                            log::error!("[IPP-USB] Failed to allocate channel IF #{}: {}", iface.number, e);
                        }
                    }
                }

                if let Ok(mut list) = opened.lock() {
                    if let Some(d) = list.iter_mut().find(|d| d.addr == addr) {
                        d.claimed_ifaces = claimed_nums;
                    }
                }

                let ch_count = channels.len();
                ipp_usb.set_channels(product_label.clone(), channels);
                if let Ok(mut st) = state.lock() {
                    st.ipp_usb_channels = ch_count;
                    st.last_event = format!(
                        "IPP-USB READY ({} channels): {}",
                        ch_count, product_label
                    );
                }

                // Quick live self-test: fetch /eSCL/ScannerCapabilities from the real Epson!
                if ch_count > 0 {
                    match ipp_usb.request_buffered("GET", "/eSCL/ScannerCapabilities", None, &[]) {
                        Ok((head, body)) => {
                            log::info!(
                                "[IPP-USB] LIVE PROBE SUCCESS! Epson /eSCL/ScannerCapabilities -> HTTP {} ({} bytes)",
                                head.status_code,
                                body.len()
                            );
                        }
                        Err(e) => {
                            log::warn!("[IPP-USB] Live probe /eSCL/ScannerCapabilities failed: {}", e);
                        }
                    }
                }
            });
    }
}

unsafe fn decode_usb_string(desc: *const sys::usb_str_desc_t) -> String {
    if desc.is_null() {
        return "(none)".to_string();
    }
    let ptr_u8 = desc as *const u8;
    let b_length = *ptr_u8 as usize;
    if b_length <= 2 {
        return "(empty)".to_string();
    }
    let byte_len = b_length - 2;
    let raw = core::slice::from_raw_parts(ptr_u8.add(2), byte_len);
    let utf16: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&utf16)
}

fn parse_config_descriptor(raw: &[u8]) -> Vec<UsbInterfaceInfo> {
    let mut interfaces: Vec<UsbInterfaceInfo> = Vec::new();
    let mut pos = 0usize;

    while pos + 2 <= raw.len() {
        let len = raw[pos] as usize;
        let desc_type = raw[pos + 1];
        if len < 2 || pos + len > raw.len() {
            break;
        }
        let d = &raw[pos..pos + len];

        match desc_type {
            0x04 if len >= 9 => {
                let class = d[5];
                let subclass = d[6];
                let protocol = d[7];
                let label = classify_interface(class, subclass, protocol);
                interfaces.push(UsbInterfaceInfo {
                    number: d[2],
                    alt_setting: d[3],
                    class,
                    subclass,
                    protocol,
                    label,
                    endpoints: Vec::new(),
                });
            }
            0x05 if len >= 7 => {
                if let Some(cur) = interfaces.last_mut() {
                    let address = d[2];
                    let attrs = d[3] & 0x03;
                    let mps = u16::from_le_bytes([d[4], d[5]]) & 0x07FF;
                    let transfer_type = match attrs {
                        0 => "Control",
                        1 => "Isochronous",
                        2 => "Bulk",
                        3 => "Interrupt",
                        _ => "Unknown",
                    };
                    cur.endpoints.push(UsbEndpointInfo {
                        address,
                        is_in: (address & 0x80) != 0,
                        transfer_type,
                        max_packet_size: mps,
                    });
                }
            }
            _ => {}
        }

        pos += len;
    }

    interfaces
}

fn classify_interface(class: u8, subclass: u8, protocol: u8) -> &'static str {
    match (class, subclass, protocol) {
        (0x07, 0x01, 0x01) => "USB Printer (Unidirectional)",
        (0x07, 0x01, 0x02) => "USB Printer (Bidirectional)",
        (0x07, 0x01, 0x03) => "USB Printer (IEEE 1284.4)",
        (0x07, 0x01, 0x04) => "IPP-USB (HTTP + AirScan/AirPrint over USB!)",
        (0x08, _, _) => "USB Mass Storage (SD/Card Reader)",
        (0x09, _, _) => "USB Hub",
        (0xFF, _, _) => "Vendor-Specific (likely Epson ESC/I Scanner)",
        _ => "Other",
    }
}
