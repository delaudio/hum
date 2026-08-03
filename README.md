# hum

`hum` is a lightweight launcher and terminal monitor for local multi-service
development environments.

Current project/template CLI:

```bash
hum compri all-services start
```

The command starts the selected services in independent process groups, writes
their logs and runtime metadata to disk, and exits. There is no resident `hum`
daemon. Later CLI invocations reconcile that metadata with the operating
system. The TUI reads the same persistent registry and controls the detached
runtime without owning service lifetimes.

> Project/template selection, v2 configuration, detached `start`, and
> cross-invocation `status`, `stop`, `restart`, and `logs`, plus external-runtime
> TUI reconciliation, are implemented. Track the remaining work in
> [epic #16](https://github.com/delaudio/hum/issues/16).

## Concepts

- **Project**: a registered local product, such as `compri`.
- **Template**: a named service selection, such as `frontend` or
  `all-services`.
- **Service**: one local process, identified at runtime by `project/service`.

## Target CLI

```bash
hum <project> <template> start
hum <project> <template> stop
hum <project> <template> restart
hum <project> <template> status
hum <project> <template> logs [service] [--lines N] [--follow]
hum <project> <template> doctor
hum <project> <template> tui
```

`hum`, `hum <project>`, and `hum <project> <template>` provide progressively
narrower interactive selection, with the final form opening the TUI.

## Configuration discovery

The binary accepts both the legacy v1 and project/template v2 formats. It discovers
`hum.yaml` from the current directory upwards, then at
`$XDG_CONFIG_HOME/hum/hum.yaml`, falling back to
`~/.config/hum/hum.yaml`. The runnable example is
[`examples/hum.example.yaml`](examples/hum.example.yaml).

The v2 contract registers projects globally in
`~/.config/hum/config.yaml` (or
`$XDG_CONFIG_HOME/hum/config.yaml`):

```yaml
version: 1

projects:
  compri:
    config: ~/code/compri/hum.yaml
```

A copyable registry shape is available at
[`examples/registry.example.yaml`](examples/registry.example.yaml).

Each project owns a versioned `hum.yaml`. A complete example is
[`examples/hum.v2.example.yaml`](examples/hum.v2.example.yaml). Machine-specific
values can live in an untracked `hum.local.yaml` beside it.

Relative paths are resolved from the configuration file that declares them.
Repository paths are relative to `hum.yaml`; service `cwd` is relative to its
repository, and `env_file` is relative to the resulting service working directory.
Service environment precedence is `.env` file, `service.env`, inherited process
environment, then repeatable `--env KEY=VALUE` CLI overrides. Unknown YAML fields
and invalid names, ports, URLs, durations, commands, or references are rejected.

## Current persistent runtime

`start` creates a dedicated session/process group for each service, redirects
stdin to null and stdout/stderr to files, writes its registry entry atomically,
then exits. State is stored below `$XDG_STATE_HOME/hum/<project>` or
`~/.local/state/hum/<project>`. The registry records PID, process group, process
start time, command identity, working directory, port, and log paths. PID
identity is verified before signals are sent. A random, inherited identity lock
keeps the whole process group recognizable even if its original shell exits.

Project operations are serialized with `project.lock`; a repeated or concurrent
`start` is idempotent. `status`, `restart`, and `stop` reconcile the same state
across independent CLI invocations, while `logs` tails the persistent stdout and
stderr files. Stop order is the reverse dependency order and its grace period is
configurable with `--timeout`. If a multi-service start partially fails, only
processes created by that invocation are rolled back. `--detach` remains accepted
as a deprecated no-op because detached execution is now the default.

Each detached service streams stdout and stderr to a small native sink that
rotates by byte count and exits automatically when both streams close. Defaults
are 10 MiB per file, three rotated files, and 64 KiB per displayed line/chunk;
all are configurable under `logs`. Raw files stay faithful to process output,
while configured regular expressions are masked in CLI/TUI views. See
[`docs/LOGGING.md`](docs/LOGGING.md) for retention, disk bounds, search, and
failure behavior.

The TUI consumes this registry in a single non-overlapping background poll,
detects processes started or stopped by other invocations, and reads persistent
log files incrementally into a bounded 500-line view. Its log window supports
scrolling, pauses live follow away from the bottom, and pages older retained
history directly from disk; `End` returns to the current tail. Closing it leaves
all services running unless the quit dialog's explicit "stop template and quit"
choice is selected; the view also has a 4 MiB byte ceiling. Doctor runs outside
the input/render loop and distinguishes managed listeners, foreign port owners,
and stale registry entries. The details view includes PID/PGID, port and health
state, command, cwd, persistent log paths, and the last exit code when the shell
wrapper can observe it. An `exec` replacement or `SIGKILL` remains explicitly
unavailable because no resident hum daemon waits on detached services. Port
polling uses bounded TCP connections; `lsof` is reserved
for explicit conflict diagnostics. Poll intervals, resource reuse, and the
ten-service CPU/RSS budget are documented in
[`docs/POLLING.md`](docs/POLLING.md).

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
scripts/verify-test-count.sh 60
cargo build --release
cargo test --release --test performance_smoke -- --ignored --nocapture
```

The detailed product contract, lifecycle, security model, migration notes, and
acceptance criteria are in [`docs/PRD.md`](docs/PRD.md).
