use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::{self, EspError};
use esp_idf_svc::wifi::{
    AuthMethod, ClientConfiguration, Configuration, EspWifi, PmfConfiguration, WifiEvent,
};

use crate::config;

#[derive(Debug, Default)]
pub struct WifiState {
    pub connected: bool,
    pub ip: Option<Ipv4Addr>,
}

/// Keeps the ESP-Hosted Wi-Fi driver (onboard ESP32-C6 over SDIO) and event subscriptions alive.
pub struct P4Wifi {
    _wifi: EspWifi<'static>,
    _wifi_sub: esp_idf_svc::eventloop::EspSubscription<'static, esp_idf_svc::eventloop::System>,
    _ip_sub: esp_idf_svc::eventloop::EspSubscription<'static, esp_idf_svc::eventloop::System>,
    pub state: Arc<Mutex<WifiState>>,
}

impl P4Wifi {
    /// Start Wi-Fi 6 (WPA3-Personal) via the onboard ESP32-C6 (`esp_wifi_remote` + `esp_hosted`).
    /// Non-blocking: connects in the background and automatically reconnects if the router reboots.
    pub fn start(
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
    ) -> Result<Self, EspError> {
        let state = Arc::new(Mutex::new(WifiState::default()));
        let want_reconnect = Arc::new(AtomicBool::new(false));

        let mut wifi = EspWifi::new(modem, sys_loop.clone(), Some(nvs))?;

        // Subscribe to Wi-Fi association/disconnect events for automatic 24/7 reconnect
        let st_wifi = state.clone();
        let reconn_flag = want_reconnect.clone();
        let wifi_sub = sys_loop.subscribe::<WifiEvent, _>(move |event| match event {
            WifiEvent::StaConnected(c) => {
                log::info!(
                    "[WiFi] Associated to \"{}\" (channel {})",
                    config::WIFI_SSID,
                    c.channel()
                );
                if let Ok(mut s) = st_wifi.lock() {
                    s.connected = true;
                }
                reconn_flag.store(false, Ordering::SeqCst);
            }
            WifiEvent::StaDisconnected(d) => {
                log::warn!(
                    "[WiFi] Disconnected (reason={}); scheduling reconnect...",
                    d.reason()
                );
                if let Ok(mut s) = st_wifi.lock() {
                    s.connected = false;
                    s.ip = None;
                }
                reconn_flag.store(true, Ordering::SeqCst);
            }
            _ => {}
        })?;

        // Subscribe to IPv4 DHCP events on Wi-Fi STA interface
        let st_ip = state.clone();
        let ip_sub = sys_loop.subscribe::<esp_idf_svc::netif::IpEvent, _>(move |event| {
            if let esp_idf_svc::netif::IpEvent::DhcpIpAssigned(assigned) = event {
                let ipv4 = Ipv4Addr::from(assigned.ip().octets());
                // Record STA IP (note: Ethernet uses its own IP_EVENT_ETH_GOT_IP handler in eth.rs)
                if let Ok(mut s) = st_ip.lock() {
                    if s.connected {
                        s.ip = Some(ipv4);
                        log::info!("========================================");
                        log::info!("[WiFi] DHCP Connected!");
                        log::info!("  IP:       {}", ipv4);
                        log::info!("  Web UI:   http://{}/", ipv4);
                        log::info!("  AirScan:  http://{}/eSCL/ScannerCapabilities", ipv4);
                        log::info!("  AirPrint: ipp://{}:631/ipp/print", ipv4);
                        log::info!("========================================");
                    }
                }
            }
        })?;

        // Configure WPA3-Personal (SAE) station
        wifi.set_configuration(&Configuration::Client(ClientConfiguration {
            ssid: config::WIFI_SSID.try_into().expect("WIFI_SSID too long"),
            password: config::WIFI_PASS.try_into().expect("WIFI_PASS too long"),
            auth_method: AuthMethod::WPA3Personal,
            pmf_cfg: PmfConfiguration::Capable { required: true },
            ..Default::default()
        }))?;

        log::info!(
            "[WiFi] Starting onboard ESP32-C6 Wi-Fi 6 (SSID=\"{}\", WPA3-SAE)...",
            config::WIFI_SSID
        );
        wifi.start()?;

        // Configure regulatory country + WPA3 SAE H2E AFTER wifi.start()
        unsafe {
            let cc_cstr = std::ffi::CString::new(config::WIFI_COUNTRY).unwrap();
            let cerr = sys::esp_wifi_set_country_code(cc_cstr.as_ptr(), true);
            log::info!(
                "[WiFi] Country code set to {} (err={})",
                config::WIFI_COUNTRY,
                cerr
            );

            let mut cfg: sys::wifi_config_t = core::mem::zeroed();
            let gerr = sys::esp_wifi_get_config(sys::wifi_interface_t_WIFI_IF_STA, &mut cfg);
            if gerr == sys::ESP_OK {
                cfg.sta.pmf_cfg.capable = true;
                cfg.sta.pmf_cfg.required = true;
                cfg.sta.sae_pwe_h2e = sys::wifi_sae_pwe_method_t_WPA3_SAE_PWE_BOTH;
                cfg.sta.sae_pk_mode = sys::wifi_sae_pk_mode_t_WPA3_SAE_PK_MODE_AUTOMATIC;
                let serr = sys::esp_wifi_set_config(sys::wifi_interface_t_WIFI_IF_STA, &mut cfg);
                let mut verify: sys::wifi_config_t = core::mem::zeroed();
                let _ = sys::esp_wifi_get_config(sys::wifi_interface_t_WIFI_IF_STA, &mut verify);
                let pass_len = verify.sta.password.iter().position(|&b| b == 0).unwrap_or(64);
                log::info!(
                    "[WiFi] C6 STA config applied (set_err={}): authmode={}, pmf=({}/{}), sae_pwe={}, pass_len={}",
                    serr,
                    verify.sta.threshold.authmode,
                    verify.sta.pmf_cfg.capable,
                    verify.sta.pmf_cfg.required,
                    verify.sta.sae_pwe_h2e,
                    pass_len
                );
            }
            // Disable Wi-Fi power-save so mDNS / AirScan / AirPrint stay low-latency & responsive!
            let _ = sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE);
        }

        // Quick scan to log target AP's exact channel, RSSI, and auth mode seen by the onboard C6
        if let Ok(aps) = wifi.scan() {
            for ap in aps {
                if ap.ssid.as_str() == config::WIFI_SSID {
                    log::info!(
                        "[WiFi] Found \"{}\" -> ch={}, rssi={} dBm, auth={:?}",
                        ap.ssid,
                        ap.channel,
                        ap.signal_strength,
                        ap.auth_method
                    );
                }
            }
        }

        // Kick off initial non-blocking connection attempt
        if let Err(e) = wifi.connect() {
            log::warn!("[WiFi] Initial connect returned {:?}; will retry in background", e);
            want_reconnect.store(true, Ordering::SeqCst);
        }

        // Background auto-reconnect watchdog thread
        thread::Builder::new()
            .name("wifi_reconn".into())
            .stack_size(4096)
            .spawn(move || loop {
                thread::sleep(Duration::from_secs(4));
                if want_reconnect.load(Ordering::SeqCst) {
                    log::info!("[WiFi] Reconnecting to \"{}\"...", config::WIFI_SSID);
                    let err = unsafe { sys::esp_wifi_connect() };
                    if err == sys::ESP_OK {
                        want_reconnect.store(false, Ordering::SeqCst);
                    } else {
                        log::warn!("[WiFi] esp_wifi_connect err={}", err);
                    }
                }
            })
            .expect("spawn wifi_reconn thread");

        Ok(Self {
            _wifi: wifi,
            _wifi_sub: wifi_sub,
            _ip_sub: ip_sub,
            state,
        })
    }
}
