# Polling budget

The ordinary monitor path is intentionally bounded for a template with ten
services:

- PID checks: every 1 s in the TUI, using targeted inspection of registry PIDs.
- Port checks: every 1 s, using a short TCP connection with a 50 ms total
  deadline across resolved IPv4/IPv6 addresses.
- HTTP/TCP health: the service-configured interval and timeout. HTTP checks
  share one connection-pooled client and checks for one service never overlap.
- Occupant diagnosis: `lsof -nP` only when an explicit CLI status or conflict
  operation requests diagnostics. The TUI
  reports a live listener as `listening-unverified` unless ownership has been
  proven elsewhere; it never runs `lsof` in a steady-state tick.
- TUI polling: at most one background polling pass is in flight; rendering and
  input are never made to wait for TCP or process inspection.

## Ten-service budget

The checked-in release smoke test starts ten detached fixture services, opens
the TUI over a pseudoterminal, samples the monitor process, exits through the
explicit leave-running path, stops every service, and verifies that no runtime
registry entries remain. Its regression thresholds are:

- ten-service detached startup: at most 2 s;
- first TUI frame: at most 250 ms (the interactive target is approximately
  200 ms);
- average idle `hum` CPU: at most 2%;
- peak `hum` resident memory: at most 60 MiB;
- resident controller/monitor daemons after `start`: zero;
- the bounded log-rotation helper: exactly one native `hum __log-sink` per
  running service, at most 1% aggregate idle CPU and 80 MiB aggregate RSS for
  ten services.

Run the automated gate with:

```bash
cargo test --release --test performance_smoke -- --ignored --nocapture
```

The CI sample lasts three seconds after a short warm-up. For a lower-noise
release measurement on a development machine, keep the TUI idle for 60 seconds:

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

The manual harness samples CPU and RSS once per second and reports average CPU
and peak RSS. Run an execution tracer alongside the sample when independently
verifying that no `lsof` process appears during steady-state polling.

The strict values above are the local acceptance budget. Shared CI runners use
explicitly documented regression ceilings (5 s startup, 1 s first frame, 5%
TUI CPU, 100 MiB TUI RSS, 3% aggregate sink CPU, and 120 MiB aggregate sink
RSS) to avoid treating host contention as a product regression. The smoke test
uses interval CPU deltas and recognizes the rendered service table, rather than
counting terminal initialization bytes as a frame.

Recorded release-build sample on 2026-08-03 (Apple Silicon macOS, ten configured
services, closed local ports, 60 one-second samples):

```text
samples=60 average_cpu=0.02% max_rss=7.53MiB
```

This fixture exercises the one-second ten-service runtime polling path and the
250 ms TUI event/redraw tick. Health-check resource reuse is covered separately by the
shared-client and non-overlap tests because the fixture intentionally has no
external network dependency.
