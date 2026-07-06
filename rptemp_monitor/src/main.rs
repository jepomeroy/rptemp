use std::{
    collections::HashMap,
    fmt::{self, Display, Formatter},
    fs::File,
    net::SocketAddr,
    path::Path,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Error;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use log::LevelFilter;
use serde::Deserialize;
use systemd_journal_logger::JournalLog;

const ENDPOINT_OUT: u8 = 0x01;
const ENDPOINT_IN: u8 = 0x82;
const SERVICE_NAME: &str = "rptemp_mon";
type SharedState = Arc<RwLock<HashMap<String, HostTemp>>>;

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct Configuration {
    usb_vendor_id: u16,
    usb_product_id: u16,
    port: u16,
    target_temp: u8,
    log_level: String,
}

/// Resolves the configured log level (e.g. "info", "debug") into a
/// `log::LevelFilter`, falling back to `Info` if it isn't a recognized name
/// so problems stay visible rather than being silently dropped.
fn resolve_log_level(log_level: &str) -> LevelFilter {
    log_level.parse().unwrap_or(LevelFilter::Info)
}

#[derive(Deserialize)]
struct HostTemp {
    host: String,
    temp: u8,
    time: u64,
}

impl Display for HostTemp {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "Host: {} reports temp of {}", self.host, self.temp)
    }
}

fn read_configuration() -> Result<Configuration, Error> {
    let base_path = Path::new("/etc");
    let path = base_path.join(SERVICE_NAME).join("config.yml");

    let file = File::open(path)?;
    let config: Configuration = yaml_serde::from_reader(file)?;
    Ok(config)
}

// GET handler health check
async fn health_check() -> (StatusCode, &'static str) {
    (StatusCode::OK, "OK")
}

// POST handler for host temp
async fn report_temp(
    State(state): State<SharedState>,
    Json(payload): Json<HostTemp>,
) -> (StatusCode, &'static str) {
    if let Ok(mut map) = state.write() {
        map.insert(payload.host.clone(), payload);

        (StatusCode::OK, "OK")
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Error processing request",
        )
    }
}

// helper functions

// Helper function to scan devices by VID and PID
fn find_device(vid: u16, pid: u16) -> Option<rusb::Device<rusb::GlobalContext>> {
    for device in rusb::devices().ok()?.iter() {
        let desc = device.device_descriptor().ok()?;
        if desc.vendor_id() == vid && desc.product_id() == pid {
            return Some(device);
        }
    }
    None
}

// Reads and logs one message from the fan controller, if any is waiting.
// A timeout with nothing to read is expected (the device only writes on
// connect or when the fan state changes), so it is not treated as an error.
// Any other error means the connection is no longer usable (e.g. the device
// was unplugged) and the caller should reconnect.
fn read_device_message(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    timeout: Duration,
) -> rusb::Result<()> {
    let mut buf = [0u8; 64];
    match handle.read_bulk(ENDPOINT_IN, &mut buf, timeout) {
        Ok(n) => {
            match std::str::from_utf8(&buf[..n]) {
                Ok(msg) => log::info!("Fan controller msg: {}", msg.trim()),
                Err(_) => log::warn!(
                    "Received non-UTF8 message from fan controller: {:?}",
                    &buf[..n]
                ),
            }
            Ok(())
        }
        Err(rusb::Error::Timeout) => Ok(()),
        Err(e) => Err(e),
    }
}

// Opens the fan controller and claims its CDC data interface, retrying
// until it succeeds. Used both for the initial connection and to recover
// after the device is unplugged and reconnected.
async fn connect_device(
    vid: u16,
    pid: u16,
    interface_num: u8,
) -> rusb::DeviceHandle<rusb::GlobalContext> {
    loop {
        match try_connect_device(vid, pid, interface_num) {
            Ok(handle) => {
                log::info!("Connected to fan controller");
                return handle;
            }
            Err(e) => {
                log::warn!("Fan controller unavailable ({e}), retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

fn try_connect_device(
    vid: u16,
    pid: u16,
    interface_num: u8,
) -> Result<rusb::DeviceHandle<rusb::GlobalContext>, String> {
    let device = find_device(vid, pid).ok_or("device not found")?;
    let handle = device
        .open()
        .map_err(|e| format!("unable to open device: {e:?}"))?;

    let kernel_active = handle
        .kernel_driver_active(interface_num)
        .map_err(|e| format!("cannot check kernel driver: {e:?}"))?;
    if kernel_active {
        handle
            .detach_kernel_driver(interface_num)
            .map_err(|e| format!("unable to detach kernel driver: {e:?}"))?;
    }

    handle
        .claim_interface(interface_num)
        .map_err(|e| format!("unable to claim interface: {e:?}"))?;

    Ok(handle)
}

fn is_host_active(host_timestamp: u64) -> bool {
    let host_time = UNIX_EPOCH + Duration::from_secs(host_timestamp);

    // Calculate the threshold: 3 minutes ago
    let threshold_time = match SystemTime::now().checked_sub(Duration::from_secs(3 * 60)) {
        Some(time) => time,
        None => return false, // Underflow (time before epoch)
    };

    // If host_time is greater than or equal to the threshold, it means the
    // timestamp was updated within the last 3 minutes.
    host_time >= threshold_time
}

#[tokio::main]
async fn main() {
    // set up host state shared object
    let host_state: SharedState = Arc::new(RwLock::new(HashMap::new()));

    // Only use journald logger if invoked by systemd
    if systemd_journal_logger::connected_to_journal() {
        JournalLog::new()
            .unwrap()
            .with_extra_fields(vec![("VERSION", env!("CARGO_PKG_VERSION"))])
            .with_syslog_identifier(SERVICE_NAME.to_string())
            .install()
            .unwrap();
    } else {
        env_logger::init();
    }

    log::set_max_level(LevelFilter::Info);

    let config = match read_configuration() {
        Ok(config) => config,
        Err(e) => {
            log::error!("Error reading configuration: {e}");
            // return default config
            Configuration {
                usb_vendor_id: 0xFFFF,
                usb_product_id: 0xFFFF,
                port: 5555,
                target_temp: 45,
                log_level: "info".to_string(),
            }
        }
    };

    log::set_max_level(resolve_log_level(&config.log_level));

    // Start processing host data
    let monitor_state = host_state.clone();

    tokio::spawn(async move {
        // Interface 1 is the CDC Data interface; it owns the bulk endpoints
        // used below. Interface 0 is the CDC Control interface (interrupt
        // endpoint only) and has no bulk endpoints to claim.
        let interface_num = 1;
        let timeout = Duration::from_secs(1);

        let mut handle =
            connect_device(config.usb_vendor_id, config.usb_product_id, interface_num).await;

        // The device sends a "CONNECTED" message as soon as the connection is
        // established, before any host write. Read it here so it doesn't sit
        // unread until the next reply-triggered read below.
        if read_device_message(&handle, timeout).is_err() {
            handle =
                connect_device(config.usb_vendor_id, config.usb_product_id, interface_num).await;
        }

        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;

            // Compute the max temp in its own scope so the RwLockReadGuard
            // (not Send) is dropped before any `.await` below.
            let max_temp = {
                let mut hosts = match monitor_state.write() {
                    Ok(hosts) => hosts,
                    Err(_) => {
                        log::error!("Error: unable to ready host state data");
                        continue;
                    }
                };

                if hosts.is_empty() {
                    log::info!("No hosts registered yet");
                }

                let mut max_temp = u8::MIN;

                hosts.retain(|host_name, host| {
                    if !is_host_active(host.time) {
                        log::info!("No response from {host_name} in over 3 minutes, removing");
                        return false;
                    }
                    log::info!("{}", host);
                    max_temp = max_temp.max(host.temp);
                    true
                });

                max_temp
            };

            let payload = max_temp.to_string();
            let write_ok = match handle.write_bulk(ENDPOINT_OUT, payload.as_bytes(), timeout) {
                Ok(_) => {
                    log::info!("Sent {} to the fan controller", max_temp);
                    true
                }
                Err(e) => {
                    log::warn!("Lost connection to fan controller ({e:?}) while writing");
                    false
                }
            };

            let read_ok = write_ok && read_device_message(&handle, timeout).is_ok();

            if !write_ok || !read_ok {
                handle = connect_device(config.usb_vendor_id, config.usb_product_id, interface_num)
                    .await;
            }
        }
    });

    // Build the API router and define paths
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/temp", post(report_temp))
        .with_state(host_state);

    // Bind the server to all network addresses on the confgured port
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    log::info!("Server running at http://{}", addr);

    // Start listening for incoming HTTP traffic
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn is_host_active_true_for_current_timestamp() {
        assert!(is_host_active(now_secs()));
    }

    #[test]
    fn is_host_active_true_for_one_minute_ago() {
        assert!(is_host_active(now_secs() - 60));
    }

    #[test]
    fn is_host_active_false_for_five_minutes_ago() {
        assert!(!is_host_active(now_secs() - 5 * 60));
    }

    #[test]
    fn is_host_active_false_for_epoch() {
        assert!(!is_host_active(0));
    }

    #[test]
    fn is_host_active_true_for_future_timestamp() {
        assert!(is_host_active(now_secs() + 60));
    }

    #[test]
    fn host_temp_display_format() {
        let host = HostTemp {
            host: "raspberrypi".to_string(),
            temp: 42,
            time: 0,
        };
        assert_eq!(format!("{host}"), "Host: raspberrypi reports temp of 42");
    }

    #[test]
    fn configuration_parses_from_yaml() {
        let yaml = "usb_vendor_id: 4292\nusb_product_id: 60000\nport: 5555\ntarget_temp: 45\nlog_level: warn\n";
        let config: Configuration = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.usb_vendor_id, 4292);
        assert_eq!(config.usb_product_id, 60000);
        assert_eq!(config.port, 5555);
        assert_eq!(config.target_temp, 45);
        assert_eq!(config.log_level, "warn");
    }

    #[test]
    fn configuration_rejects_missing_log_level() {
        let yaml = "usb_vendor_id: 4292\nusb_product_id: 60000\nport: 5555\ntarget_temp: 45\n";
        assert!(yaml_serde::from_str::<Configuration>(yaml).is_err());
    }

    #[test]
    fn resolve_log_level_parses_valid_level() {
        assert_eq!(resolve_log_level("debug"), LevelFilter::Debug);
    }

    #[test]
    fn resolve_log_level_is_case_insensitive() {
        assert_eq!(resolve_log_level("WARN"), LevelFilter::Warn);
    }

    #[test]
    fn resolve_log_level_defaults_to_info_for_invalid_name() {
        assert_eq!(resolve_log_level("bogus"), LevelFilter::Info);
    }

    #[tokio::test]
    async fn health_check_returns_ok() {
        let (status, body) = health_check().await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "OK");
    }

    #[tokio::test]
    async fn report_temp_stores_host_and_returns_ok() {
        let state: SharedState = Arc::new(RwLock::new(HashMap::new()));

        let payload = HostTemp {
            host: "host-a".to_string(),
            temp: 55,
            time: now_secs(),
        };
        let (status, body) = report_temp(State(state.clone()), Json(payload)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "OK");

        let hosts = state.read().unwrap();
        assert_eq!(hosts.get("host-a").unwrap().temp, 55);
    }

    #[tokio::test]
    async fn report_temp_overwrites_existing_host() {
        let state: SharedState = Arc::new(RwLock::new(HashMap::new()));

        for temp in [10, 20] {
            let payload = HostTemp {
                host: "host-b".to_string(),
                temp,
                time: now_secs(),
            };
            report_temp(State(state.clone()), Json(payload)).await;
        }

        let hosts = state.read().unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts.get("host-b").unwrap().temp, 20);
    }

    #[tokio::test]
    async fn report_temp_returns_500_when_lock_is_poisoned() {
        let state: SharedState = Arc::new(RwLock::new(HashMap::new()));

        let poison_state = state.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poison_state.write().unwrap();
            panic!("intentionally poisoning the lock for the test");
        })
        .join();
        assert!(state.is_poisoned());

        let payload = HostTemp {
            host: "host-c".to_string(),
            temp: 30,
            time: now_secs(),
        };
        let (status, body) = report_temp(State(state.clone()), Json(payload)).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Error processing request");
    }
}
