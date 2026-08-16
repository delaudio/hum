# Persistent logging

`hum start` redirects each process service's stdout and stderr to two pipes
consumed by one small native log sink. It rotates files, drains both streams,
and exits at EOF after the service process group closes. CLI and TUI readers are
independent, so closing either reader cannot deliver `SIGPIPE` to the service.

## Configuration

```yaml
logs:
  max_file_bytes: 10485760
  rotated_files: 3
  max_line_bytes: 65536
  retention: 7d
  redact_patterns:
    - "(?i)(token|password)=[^ ]+"
  exporters:
    - type: http
      endpoint: http://127.0.0.1:8687/events
      timeout: 750ms
      headers:
        Authorization: "Bearer machine-local-token"
```

- `max_file_bytes` is the exact upper bound for each active or rotated stream
  file and must be greater than zero.
- `rotated_files` is the number of historical files retained per stream
  (`.1` is newest); zero keeps only the active file.
- `max_line_bytes` bounds partial lines and newline-free chunks in followers.
- `retention` is optional. Expired rotated files are removed when a sink starts
  and after rotations. File-count cleanup is always active.
- `redact_patterns` contains regular expressions applied only when logs are
  displayed or exported. The raw files are not rewritten. Configuration is
  bounded to 128 expressions and 256 KiB of aggregate pattern text.
- `exporters` optionally forwards a redacted NDJSON copy of process-runtime
  logs. HTTP endpoints must not embed credentials, queries, or fragments;
  plaintext HTTP is accepted only on loopback (`localhost`, `127.0.0.1`, or
  `::1`); private/LAN addresses such as `10.x` and `192.168.x` still require
  HTTPS. Names ending in `.localhost` follow the RFC 6761 loopback-name
  contract and are not DNS-resolved by Hum. At most 16 exporters are accepted, endpoint URLs are capped at 8 KiB,
  and the request timeout must be at most 10 seconds. `headers` is an optional
  map of static HTTP request headers, limited to 16 entries and 8 KiB in total.
  Header names and values are validated before startup.

Static header values may contain collector credentials. Keep those values in
an untracked, owner-readable `hum.local.yaml` rather than the public project
configuration, and avoid printing that local configuration in shared output.
Hum sends configured headers as-is and does not source or rotate them.
For detached process sinks, Hum serializes exporter configuration through a
pathless Unix socket inherited as a private descriptor; credential-bearing
headers are never written to a named temporary file or command-line argument.
Before reading, the sink verifies that the inherited descriptor is both a
stream socket and part of the Unix address family.
Readiness uses a second pathless Unix socket, independent from the private
configuration channel. The parent allows at most two seconds for non-blocking
configuration delivery and three seconds for the one-byte readiness
acknowledgement. The sink sends that acknowledgement after classifying the
configuration and before reading any service output; the parent constructs the
service process only after receiving it. A missing, mistyped, truncated, or
invalid configuration channel disables only optional export and adds a static
diagnostic to persistent stderr; stream capture remains active. A timed-out or
failed parent write also emits a static non-fatal CLI warning after readiness.
If the independent readiness channel itself fails, Hum does not start a service
whose persistent capture is unavailable.

For one service with both streams, the configured hard disk bound is:

```text
2 × (max_file_bytes + 17) × (rotated_files + 1)
```

The additional 17 bytes per generation are a bounded internal marker used to
bind the file identity and prove whether it begins at a complete line boundary;
filesystem allocation metadata is not included in this logical-byte bound.

Writes are split at the byte boundary, including very long lines. Readers join
verified fragments across rotations; incomplete history boundaries are dropped,
and lines over `max_line_bytes` are shown only as omission markers. If persistence
fails (for example because the disk is full), the sink keeps draining and
discarding output so a noisy service is not terminated by a closed pipe.

## Best-effort export

Each exported event contains `@timestamp`, `message`, `stream`,
`service.name`, `hum.project`, and `hum.runtime=process`. Lines larger than
`max_line_bytes` become an omission marker before export. Events use
newline-delimited JSON and are sent in bounded batches. Serialized events over
60 KiB become an omission marker so queue memory and request bodies stay
bounded independently of the configured display limit. If fixed project or
service metadata alone exceeds that bound, Hum rejects the event and records a
coalesced persistent diagnostic instead of silently dropping it. Across all
configured exporters, Hum allocates at most 256 queued-event slots and 64
concurrent batch-event slots; those budgets are divided among exporters that
initialized successfully, so an invalid exporter does not reduce a surviving
worker's share. With 16 successful exporters, for example, each worker receives
16 queue slots and four batch-event slots. Other worker counts use integer
division, leaving any remainder unallocated while retaining at least one slot
per worker. Export payload
memory is therefore bounded below 19 MiB plus fixed channel, HTTP-client,
and thread overhead. A request contains at most its exporter's share of the
64-event batch budget, including during the final shutdown flush.
Non-UTF-8 process output remains byte-faithful in persistent files, while its
optional JSON export uses Unicode replacement characters for invalid sequences.
Redaction patterns run against that replacement-character view, so matching
across invalid UTF-8 boundaries is intentionally best-effort.
Enabling an exporter also performs UTF-8 conversion and configured regex
redaction once per emitted line; leave export disabled for extremely noisy
services unless that CPU trade-off is acceptable.

Export is deliberately lossy and cannot backpressure stdout or stderr. Each
endpoint has a bounded queue; new events are dropped while the queue is full,
and a failed HTTP batch is not requeued. Endpoint recovery is observed only as
later batches arrive. After a failure, that endpoint's worker pauses for 250 ms;
during a sustained outage the queue therefore fills quickly and additional
events are intentionally dropped. HTTP batch failures and queue drops both
produce worker-attributed diagnostics. Hum writes a bounded diagnostic with the
drop count directly to the service's persistent stderr log, coalesced to at
most one diagnostic write per second; that diagnostic is not fed back into the
exporter. With multiple exporters, each drop diagnostic identifies the
one-based worker position in configuration order. A worker panic is reported
once as a distinct worker-termination diagnostic; subsequent events are no
longer submitted to or counted against that permanently failed worker. A
disconnected queue without a recorded panic remains attributed as a dropped
event. Persistent Hum
files, CLI logs, and the TUI remain available regardless of exporter state. If
an internal redaction specification cannot be compiled, Hum likewise records a
static stderr diagnostic and disables only optional export.
Worker initialization failures, including any validation/runtime mismatch in
static headers, use the same persistent diagnostic path.
Each configured HTTP exporter owns one worker thread with a single-threaded
Tokio runtime for its asynchronous HTTP client.
On service exit, the sink gives concurrent exporter workers a bounded grace
period (at most two seconds total, independently of a longer request timeout)
to flush final events. The final 50 ms are reserved for cooperative cancellation
and joining of in-flight exporter workers, so the sink never waits indefinitely
for a collector. The private sink then exits its process explicitly, which is
the hard boundary for any OS HTTP operation that ignored cooperative thread
cancellation.
This makes an exporter suitable for optional local tools such as Vector or a
generic local HTTP collector without making it a runtime dependency.

Compose services continue to use runtime-native logs. A product pack that wants
searchable Docker logs should configure its collector against the Compose
runtime directly, while using this exporter for host-native process services.

## Reading logs

```bash
# every service in the selected template
hum demo all-services logs --lines 100

# one service, continuously across rotations
hum demo all-services logs api --lines 100 --follow
```

The CLI reads at most 512 KiB per stream for an initial tail and marks a tail
that had to be truncated. In the TUI, `l` opens a byte-bounded incremental view.
Use the arrows or `j`/`k` to scroll, Page Up/Page Down for larger steps, `Home`
to progressively load older retained pages from disk, and `End` to return to the
live tail. Scrolling up pauses live follow so incoming output cannot move the
viewport; returning to the bottom reloads the current tail. `/` searches the
current view, and `c` clears only the view. None of these actions deletes or
truncates persistent files; cleanup is exclusively controlled by the rotation
and retention policy above.
