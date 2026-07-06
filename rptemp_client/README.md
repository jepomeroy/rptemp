# rptemp_client

A small daemon that reads the host's CPU temperature and reports it to a
[rptemp_monitor](../rptemp_monitor/README.md) instance over HTTP.

## How it works

- Every `report_freq` seconds (from configuration) the client:
  - refreshes the system's hardware sensor list and finds the component
    whose label contains "cpu",
  - rounds its temperature to a whole degree,
  - `POST`s `{"host": "<hostname>", "temp": <u8>, "time": <unix-seconds>}` to
    the configured monitor's `/temp` endpoint.
- The hostname is read once at startup via `gethostname`.
- The connection is closed after each report (`Connection: close`), and the
  client reads back the response status line before moving on, so the
  socket isn't torn down mid-write on the server's end.

## Configuration

On startup the client reads `/etc/rptemp_clnt/config.yml`. If the file is
missing or invalid (including a missing/unrecognized `log_level`), it logs
an error and falls back to a default configuration (`localhost:5555`, 30s
report frequency, `info` logging), so it will keep attempting reports even
without a valid configuration.

```yaml
monitor_host: localhost   # hostname or IP of the rptemp_monitor instance
monitor_port: 5555        # TCP port rptemp_monitor is listening on
report_freq: 30           # Seconds between temperature reports
log_level: info           # off, error, warn, info, debug, or trace
```

See [`example/`](example) for a sample config and systemd unit.

## Running

```bash
cargo run
```

Logs go to the systemd journal when run under systemd, or to stderr
otherwise (via `env_logger`).

## Testing

```bash
cargo test
```

Unit tests cover the pure logic split out of `main`: temperature rounding
(including saturation for out-of-range/NaN readings), HTTP request
formatting, response status parsing, and config YAML parsing. The network
and hardware-sensor I/O in `main` itself isn't covered by these tests.
