# Persistent logging

`hum start` redirects each service's stdout and stderr to two pipes consumed by
one small native log sink. The sink has no Tokio runtime, project manager, or
in-memory history. It rotates files, drains both streams, and exits at EOF after
the service process group closes. CLI and TUI readers are independent, so
closing either reader cannot deliver `SIGPIPE` to the service.

## Configuration

```yaml
logs:
  max_file_bytes: 10485760
  rotated_files: 3
  max_line_bytes: 65536
  retention: 7d
  redact_patterns:
    - "(?i)(token|password)=[^ ]+"
```

- `max_file_bytes` is the exact upper bound for each active or rotated stream
  file and must be greater than zero.
- `rotated_files` is the number of historical files retained per stream
  (`.1` is newest); zero keeps only the active file.
- `max_line_bytes` bounds partial lines and newline-free chunks in followers.
- `retention` is optional. Expired rotated files are removed when a sink starts
  and after rotations. File-count cleanup is always active.
- `redact_patterns` contains regular expressions applied only when logs are
  displayed. The raw files are not rewritten.

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

## Reading logs

```bash
# every service in the selected template
hum compri all-services logs --lines 100

# one service, continuously across rotations
hum compri all-services logs api --lines 100 --follow
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
