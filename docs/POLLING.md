# Polling budget

The ordinary monitor path is intentionally bounded for a template with ten
services:

- PID checks: every 500 ms, using only already-owned process handles or a
  targeted refresh of a known PID.
- Port checks: every 1 s, using a short TCP connection with a 50 ms total
  deadline across resolved IPv4/IPv6 addresses.
- HTTP/TCP health: the service-configured interval and timeout. HTTP checks
  share one connection-pooled client and checks for one service never overlap.
- Occupant diagnosis: `lsof -nP` only when a listener first appears in an
  unknown state or when an explicit CLI operation requests diagnostics. It is
  not part of a steady-state tick.
- TUI polling: at most one background polling pass is in flight; rendering and
  input are never made to wait for TCP or process inspection.

## Ten-service budget

For a release build on an Apple Silicon development machine, the acceptance
budget while the TUI is idle for 60 seconds is:

- average `hum` CPU: at most 2%;
- `hum` resident memory: at most 60 MiB;
- external processes spawned by ordinary polling: zero.

Reproduce the sample in two terminals with the checked-in ten-service fixture:

```bash
cargo build --release
target/release/hum --config examples/hum.polling-benchmark.yaml polling-benchmark all tui

# second terminal; use the PID shown by ps/pgrep
scripts/measure-polling-budget.sh <hum-pid> 60
```

The harness samples CPU and RSS once per second and reports average CPU and peak
RSS. Run an execution tracer alongside the sample when independently verifying
that no `lsof` process appears during steady-state polling.

Recorded release-build sample on 2026-08-03 (Apple Silicon macOS, ten configured
services, closed local ports, 60 one-second samples):

```text
samples=60 average_cpu=0.02% max_rss=7.53MiB
```

This fixture exercises the one-second ten-port polling path and the 500 ms TUI
redraw/PID schedule. Health-check resource reuse is covered separately by the
shared-client and non-overlap tests because the fixture intentionally has no
external network dependency.
