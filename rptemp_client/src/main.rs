use std::{
    fmt::{self, Display, Formatter},
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::Path,
    process::ExitCode,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Error;
use gethostname::gethostname;
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use sysinfo::Components;
use systemd_journal_logger::JournalLog;

const SERVICE_NAME: &str = "rptemp_clnt";

#[derive(Debug, PartialEq, Deserialize)]
struct Configuration {
    monitor_host: String,
    monitor_port: u16,
    report_freq: u64,
    log_level: String,
}

/// Resolves the configured log level (e.g. "info", "debug") into a
/// `log::LevelFilter`, falling back to `Info` if it isn't a recognized name
/// so problems stay visible rather than being silently dropped.
fn resolve_log_level(log_level: &str) -> LevelFilter {
    log_level.parse().unwrap_or(LevelFilter::Info)
}

#[derive(Serialize)]
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

fn parse_configuration<R: Read>(reader: R) -> Result<Configuration, Error> {
    let config: Configuration = yaml_serde::from_reader(reader)?;
    Ok(config)
}

fn read_configuration() -> Result<Configuration, Error> {
    let base_path = Path::new("/etc");
    let path = base_path.join(SERVICE_NAME).join("config.yml");

    let file = File::open(path)?;
    parse_configuration(file)
}

/// Rounds a sensor reading to the nearest whole degree, saturating into `u8`
/// range rather than wrapping (Rust's `as` cast saturates float-to-int since
/// 1.45, so e.g. negative readings clamp to 0 instead of wrapping to 255).
fn round_temp(temp: f32) -> u8 {
    temp.round() as u8
}

/// Builds the raw HTTP/1.1 request sent to the monitor's `/temp` endpoint.
fn build_http_request(host: &str, port: u16, body: &str) -> String {
    format!(
        "POST /temp HTTP/1.1\r\n\
         Host: {}:{}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {}",
        host,
        port,
        body.len(),
        body
    )
}

/// Whether a response status line indicates the monitor accepted the report.
fn is_success_status(status_line: &str) -> bool {
    status_line.starts_with("HTTP/1.1 200")
}

fn main() -> ExitCode {
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
                monitor_host: "localhost".to_string(),
                monitor_port: 5555,
                report_freq: 30,
                log_level: "info".to_string(),
            }
        }
    };

    log::set_max_level(resolve_log_level(&config.log_level));

    let hostname: String = match gethostname().into_string() {
        Ok(host) => host,
        Err(_) => {
            log::error!("Could not read hostname");
            return ExitCode::FAILURE;
        }
    };

    let mut payload = HostTemp {
        host: hostname.clone(),
        temp: 0,
        time: 0,
    };

    loop {
        // Initialize and refresh the components list
        let components = Components::new_with_refreshed_list();

        for component in &components {
            if !component.label().to_lowercase().contains("cpu") {
                continue;
            }
            // Temperature is returned as an Option<f32> in Celsius; round to whole degrees for HostTemp's u8 field
            if let Some(temperature) = component.temperature() {
                let temp: u8 = round_temp(temperature);
                let current_timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_secs(); // Returns a u64

                payload.temp = temp;
                payload.time = current_timestamp;

                match TcpStream::connect(format!("{}:{}", config.monitor_host, config.monitor_port))
                {
                    Ok(mut stream) => {
                        let body = match serde_json::to_string(&payload) {
                            Ok(body) => body,
                            Err(e) => {
                                log::error!("Failed to serialize payload: {e}");
                                continue;
                            }
                        };

                        let request =
                            build_http_request(&config.monitor_host, config.monitor_port, &body);

                        let sent = stream
                            .write_all(request.as_bytes())
                            .and_then(|_| stream.flush());

                        if let Err(e) = sent {
                            log::error!("Failed to send temp report: {e}");
                            continue;
                        }

                        // Read the status line so the connection isn't torn
                        // down before the server has written its response;
                        // closing immediately after write races with the
                        // server's reply and can silently drop it.
                        let mut status_line = String::new();
                        match BufReader::new(&stream).read_line(&mut status_line) {
                            Ok(_) if is_success_status(&status_line) => {
                                log::info!("Sent temp report successfully")
                            }
                            Ok(_) => {
                                log::error!("Monitor rejected temp report: {}", status_line.trim())
                            }
                            Err(e) => log::error!("Failed to read monitor response: {e}"),
                        }
                    }
                    Err(e) => log::error!(
                        "Could not connect to {}:{}: {e}",
                        config.monitor_host,
                        config.monitor_port
                    ),
                }
            }
        }

        thread::sleep(Duration::from_secs(config.report_freq));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_temp_rounds_to_nearest_degree() {
        assert_eq!(round_temp(45.4), 45);
        assert_eq!(round_temp(45.5), 46);
        assert_eq!(round_temp(45.6), 46);
    }

    #[test]
    fn round_temp_saturates_below_zero() {
        assert_eq!(round_temp(-5.0), 0);
    }

    #[test]
    fn round_temp_saturates_above_u8_max() {
        assert_eq!(round_temp(300.0), 255);
    }

    #[test]
    fn round_temp_handles_nan() {
        // Rust's float-to-int `as` cast maps NaN to 0.
        assert_eq!(round_temp(f32::NAN), 0);
    }

    #[test]
    fn host_temp_display_formats_host_and_temp() {
        let payload = HostTemp {
            host: "raspberrypi".to_string(),
            temp: 42,
            time: 0,
        };
        assert_eq!(payload.to_string(), "Host: raspberrypi reports temp of 42");
    }

    #[test]
    fn build_http_request_includes_expected_headers_and_body() {
        let body = r#"{"host":"pi","temp":42,"time":0}"#;
        let request = build_http_request("192.168.1.10", 5555, body);

        assert!(request.starts_with("POST /temp HTTP/1.1\r\n"));
        assert!(request.contains("Host: 192.168.1.10:5555\r\n"));
        assert!(request.contains("Content-Type: application/json\r\n"));
        assert!(request.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(request.contains("Connection: close\r\n"));
        assert!(request.ends_with(body));
    }

    #[test]
    fn is_success_status_accepts_http_200() {
        assert!(is_success_status("HTTP/1.1 200 OK\r\n"));
    }

    #[test]
    fn is_success_status_rejects_non_200() {
        assert!(!is_success_status("HTTP/1.1 404 Not Found\r\n"));
        assert!(!is_success_status("HTTP/1.1 500 Internal Server Error\r\n"));
    }

    #[test]
    fn is_success_status_rejects_empty_or_garbage() {
        assert!(!is_success_status(""));
        assert!(!is_success_status("not a status line"));
    }

    #[test]
    fn parse_configuration_reads_valid_yaml() {
        let yaml =
            "monitor_host: 192.168.1.10\nmonitor_port: 5555\nreport_freq: 30\nlog_level: warn\n";
        let config = parse_configuration(yaml.as_bytes()).unwrap();
        assert_eq!(
            config,
            Configuration {
                monitor_host: "192.168.1.10".to_string(),
                monitor_port: 5555,
                report_freq: 30,
                log_level: "warn".to_string(),
            }
        );
    }

    #[test]
    fn parse_configuration_rejects_missing_field() {
        let yaml = "monitor_host: 192.168.1.10\nmonitor_port: 5555\n";
        assert!(parse_configuration(yaml.as_bytes()).is_err());
    }

    #[test]
    fn parse_configuration_rejects_missing_log_level() {
        let yaml = "monitor_host: 192.168.1.10\nmonitor_port: 5555\nreport_freq: 30\n";
        assert!(parse_configuration(yaml.as_bytes()).is_err());
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

    #[test]
    fn parse_configuration_rejects_malformed_yaml() {
        let yaml = "not: [valid, yaml: at all";
        assert!(parse_configuration(yaml.as_bytes()).is_err());
    }
}
