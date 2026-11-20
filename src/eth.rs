use std::net::Ipv4Addr;
use std::ptr;
use std::sync::{Arc, Mutex};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::sys::{self, esp, EspError};

/// Waveshare ESP32-P4-Module-DEV-KIT RMII Ethernet pin mapping (IP101GRI PHY)
pub mod waveshare_p4_pins {
    pub const MDC: i32 = 31;
    pub const MDIO: i32 = 52;
    pub const REF_CLK: i32 = 50;
    pub const TX_EN: i32 = 49;
    pub const TXD0: i32 = 34;
    pub const TXD1: i32 = 35;
    pub const CRS_DV: i32 = 28;
    pub const RXD0: i32 = 29;
    pub const RXD1: i32 = 30;
    pub const PHY_RST: i32 = 51;
}

/// Shared network status updated from ESP-IDF Ethernet & IP events.
#[derive(Debug, Default)]
pub struct NetworkState {
    pub link_up: bool,
    pub ip: Option<Ipv4Addr>,
}

/// Holds the active ESP-IDF Ethernet handle, netif, and event handlers alive.
pub struct P4Ethernet {
    _eth_handle: sys::esp_eth_handle_t,
    _eth_netif: *mut sys::esp_netif_t,
    _eth_glue: sys::esp_eth_netif_glue_handle_t,
    pub state: Arc<Mutex<NetworkState>>,
}

// Handles are managed by ESP-IDF internals and only kept alive by P4Ethernet.
unsafe impl Send for P4Ethernet {}

impl P4Ethernet {
    /// Initialize ESP32-P4 internal EMAC + IP101GRI PHY + lwIP DHCP netif.
    /// Does NOT block if no Ethernet cable is plugged in yet; plugging a cable in
    /// at any time will trigger link-up + DHCP automatically.
    pub fn start(_sys_loop: &EspSystemEventLoop) -> Result<Self, EspError> {
        let state = Arc::new(Mutex::new(NetworkState::default()));

        unsafe {
            // Ensure TCP/IP stack is initialized (safe to call if already initialized).
            let err = sys::esp_netif_init();
            if err != sys::ESP_OK && err != sys::ESP_ERR_INVALID_STATE {
                esp!(err)?;
            }

            // Create default Ethernet netif (DHCP client enabled by default)
            let netif_cfg = sys::esp_netif_config_t {
                base: &sys::_g_esp_netif_inherent_eth_config,
                driver: ptr::null(),
                stack: sys::_g_esp_netif_netstack_default_eth,
            };
            let eth_netif = sys::esp_netif_new(&netif_cfg);
            if eth_netif.is_null() {
                return Err(EspError::from_infallible::<{ sys::ESP_FAIL }>());
            }

            // Configure ESP32-P4 internal EMAC with Waveshare RMII pins
            let mut emac_cfg: sys::eth_esp32_emac_config_t = core::mem::zeroed();
            emac_cfg.__bindgen_anon_1.smi_gpio = sys::emac_esp_smi_gpio_config_t {
                mdc_num: waveshare_p4_pins::MDC,
                mdio_num: waveshare_p4_pins::MDIO,
            };
            emac_cfg.interface = sys::eth_data_interface_t_EMAC_DATA_INTERFACE_RMII;
            emac_cfg.clock_config.rmii.clock_mode = sys::emac_rmii_clock_mode_t_EMAC_CLK_EXT_IN;
            emac_cfg.clock_config.rmii.clock_gpio = waveshare_p4_pins::REF_CLK;
            emac_cfg.dma_burst_len = sys::eth_mac_dma_burst_len_t_ETH_DMA_BURST_LEN_32;
            emac_cfg.intr_priority = 0;
            emac_cfg.mdc_freq_hz = 0;
            emac_cfg.emac_dataif_gpio.rmii = sys::eth_mac_rmii_gpio_config_t {
                tx_en_num: waveshare_p4_pins::TX_EN,
                txd0_num: waveshare_p4_pins::TXD0,
                txd1_num: waveshare_p4_pins::TXD1,
                crs_dv_num: waveshare_p4_pins::CRS_DV,
                rxd0_num: waveshare_p4_pins::RXD0,
                rxd1_num: waveshare_p4_pins::RXD1,
            };

            let mac_cfg = sys::eth_mac_config_t {
                sw_reset_timeout_ms: 100,
                rx_task_stack_size: 4096,
                rx_task_prio: 15,
                flags: 0,
            };

            let mac = sys::esp_eth_mac_new_esp32(&emac_cfg, &mac_cfg);
            if mac.is_null() {
                return Err(EspError::from_infallible::<{ sys::ESP_FAIL }>());
            }

            // Configure IP101GRI PHY (auto-probe address, reset on GPIO 51)
            let phy_cfg = sys::eth_phy_config_t {
                phy_addr: sys::ESP_ETH_PHY_ADDR_AUTO,
                reset_timeout_ms: 100,
                autonego_timeout_ms: 4000,
                reset_gpio_num: waveshare_p4_pins::PHY_RST,
                hw_reset_assert_time_us: 0,
                post_hw_reset_delay_ms: 0,
            };

            let phy = sys::esp_eth_phy_new_ip101(&phy_cfg);
            if phy.is_null() {
                return Err(EspError::from_infallible::<{ sys::ESP_FAIL }>());
            }

            // Install Ethernet driver
            let mut eth_cfg: sys::esp_eth_config_t = core::mem::zeroed();
            eth_cfg.mac = mac;
            eth_cfg.phy = phy;
            eth_cfg.check_link_period_ms = 2000;

            let mut eth_handle: sys::esp_eth_handle_t = ptr::null_mut();
            esp!(sys::esp_eth_driver_install(&eth_cfg, &mut eth_handle))?;

            // Attach Ethernet driver to TCP/IP netif
            let eth_glue = sys::esp_eth_new_netif_glue(eth_handle);
            esp!(sys::esp_netif_attach(eth_netif, eth_glue as *mut _))?;

            // Register Ethernet and IP event handlers
            let state_ptr = Arc::into_raw(state.clone()) as *mut core::ffi::c_void;
            esp!(sys::esp_event_handler_register(
                sys::ETH_EVENT,
                sys::ESP_EVENT_ANY_ID,
                Some(eth_event_handler),
                state_ptr,
            ))?;
            esp!(sys::esp_event_handler_register(
                sys::IP_EVENT,
                sys::ip_event_t_IP_EVENT_ETH_GOT_IP as i32,
                Some(ip_event_handler),
                state_ptr,
            ))?;

            // Start Ethernet driver (non-blocking)
            esp!(sys::esp_eth_start(eth_handle))?;

            log::info!("Ethernet EMAC + IP101GRI PHY started (waiting for RJ45 link)");

            Ok(Self {
                _eth_handle: eth_handle,
                _eth_netif: eth_netif,
                _eth_glue: eth_glue,
                state,
            })
        }
    }

    #[allow(dead_code)]
    pub fn current_ip(&self) -> Option<Ipv4Addr> {
        self.state.lock().unwrap().ip
    }
}

unsafe extern "C" fn eth_event_handler(
    arg: *mut core::ffi::c_void,
    _event_base: sys::esp_event_base_t,
    event_id: i32,
    _event_data: *mut core::ffi::c_void,
) {
    let state = &*(arg as *const Mutex<NetworkState>);
    match event_id as u32 {
        sys::eth_event_t_ETHERNET_EVENT_CONNECTED => {
            log::info!(">>> ETH EVENT: Link UP (10/100M RJ45 connected)");
            if let Ok(mut s) = state.lock() {
                s.link_up = true;
            }
        }
        sys::eth_event_t_ETHERNET_EVENT_DISCONNECTED => {
            log::warn!(">>> ETH EVENT: Link DOWN (RJ45 cable unplugged)");
            if let Ok(mut s) = state.lock() {
                s.link_up = false;
                s.ip = None;
            }
        }
        sys::eth_event_t_ETHERNET_EVENT_START => {
            log::info!(">>> ETH EVENT: Driver started");
        }
        sys::eth_event_t_ETHERNET_EVENT_STOP => {
            log::info!(">>> ETH EVENT: Driver stopped");
        }
        _ => {}
    }
}

unsafe extern "C" fn ip_event_handler(
    arg: *mut core::ffi::c_void,
    _event_base: sys::esp_event_base_t,
    _event_id: i32,
    event_data: *mut core::ffi::c_void,
) {
    let state = &*(arg as *const Mutex<NetworkState>);
    let event = &*(event_data as *const sys::ip_event_got_ip_t);
    let ip = Ipv4Addr::from(u32::from_be(event.ip_info.ip.addr));
    let mask = Ipv4Addr::from(u32::from_be(event.ip_info.netmask.addr));
    let gw = Ipv4Addr::from(u32::from_be(event.ip_info.gw.addr));

    if let Ok(mut s) = state.lock() {
        s.ip = Some(ip);
    }

    log::info!("========================================");
    log::info!("Ethernet DHCP Connected!");
    log::info!("  IP:      {}", ip);
    log::info!("  Netmask: {}", mask);
    log::info!("  Gateway: {}", gw);
    log::info!("  Web UI:  http://{}/", ip);
    log::info!("  AirScan: http://{}/eSCL/ScannerCapabilities", ip);
    log::info!("  AirPrint: ipp://{}:631/ipp/print", ip);
    log::info!("========================================");
}
