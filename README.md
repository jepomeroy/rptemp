# rptemp

A two-part system for keeping an eye on host temperatures and cooling them
with a USB-connected fan controller.

- **[rptemp_client](rptemp_client/README.md)** runs on each host you want to
  monitor. It reads the host's CPU temperature on a configurable interval and
  reports it to a monitor instance over HTTP.
- **[rptemp_monitor](rptemp_monitor/README.md)** runs on the host with the
  fan controller attached. It aggregates temperature reports from all
  clients, drops any host that's stopped reporting, and drives the fan
  controller based on the hottest host currently reporting in.

```
 rptemp_client (host A) ─┐
 rptemp_client (host B) ─┼─ POST /temp ─▶ rptemp_monitor ─▶ USB fan controller
 rptemp_client (host C) ─┘
```

Each project has its own README with configuration, running, and testing
details, plus an `example/` directory with a sample YAML config and systemd
unit:

- [rptemp_client/README.md](rptemp_client/README.md) /
  [rptemp_client/example](rptemp_client/example)
- [rptemp_monitor/README.md](rptemp_monitor/README.md) /
  [rptemp_monitor/example](rptemp_monitor/example)
