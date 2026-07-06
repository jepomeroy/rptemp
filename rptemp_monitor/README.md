# rptemp_monitor

A small HTTP service that aggregates temperature reports from one or more
remote hosts and drives a USB-connected fan controller based on the hottest
host currently reporting in.

## How it works

- Remote hosts periodically `POST` their temperature to `/temp`. Each report
  is keyed by hostname and stamped with the time it was received.
- Every 15 seconds the service:
  - drops any host that hasn't reported in over 3 minutes (considered
    offline),
  - computes the maximum temperature across all remaining hosts,
  - writes that value to a USB fan controller over a CDC (USB-serial) bulk
    endpoint.
- The fan controller is connected to over USB using the vendor/product ID
  from the configuration file. If the device is missing or is unplugged
  while running, the service retries the connection every 5 seconds and
  resumes automatically once it reappears.
- `GET /health` reports service health.

## Configuration

On startup the service reads `/etc/rptemp_mon/config.yml`. If the file is
missing or invalid (including a missing/unrecognized `log_level`), it logs
an error and falls back to a default configuration (vendor/product
`0xFFFF`/`0xFFFF`, port `5555`, target temp `45`, `info` logging), so it can
still serve `/health` even without a valid configuration.

```yaml
usb_vendor_id: 4292      # USB vendor ID of the fan controller (decimal or 0x-hex)
usb_product_id: 60000    # USB product ID of the fan controller
port: 5555               # TCP port the HTTP server listens on
target_temp: 45          # Reserved for future use; not yet consumed by this service
log_level: info          # off, error, warn, info, debug, or trace
```

See [`example/`](example) for a sample config and systemd unit.

## Running

```bash
cargo run
```

The service binds to `0.0.0.0:<port>` and logs to the systemd journal when
run under systemd, or to stderr otherwise (via `env_logger`).

## API

| Method | Path      | Body                                          | Description                          |
|--------|-----------|------------------------------------------------|---------------------------------------|
| GET    | `/health` | -                                              | Returns `200 OK` if the service is up |
| POST   | `/temp`   | `{"host": "<name>", "temp": <u8>, "time": <unix-seconds>}` | Reports a host's current temperature  |

## Testing

Unit tests live inline in `src/main.rs` under `#[cfg(test)] mod tests` and
cover:

- `is_host_active` host-staleness logic
- `HostTemp`'s `Display` formatting
- YAML deserialization of `Configuration`
- the `/health` and `/temp` HTTP handlers, including the shared-state
  poisoned-lock error path

Run them with:

```bash
cargo test
```

USB device discovery/connection (`find_device`, `connect_device`,
`try_connect_device`, `read_device_message`) talks directly to real USB
hardware via `rusb` and is not covered by unit tests, since it requires a
physical fan controller to exercise.
