# hum

> A product-neutral launcher, dependency orchestrator, and terminal monitor for local multi-service development environments.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![CI](https://github.com/delaudio/hum/actions/workflows/ci.yml/badge.svg)](https://github.com/delaudio/hum/actions/workflows/ci.yml)
[![Platform](https://img.shields.io/badge/Platform-macOS%20%7C%20Linux-lightgrey.svg)]()

`hum` is a fast, lightweight local development orchestrator and terminal monitor built with **Rust**, **Ratatui**, **Crossterm**, and **Tokio**. It coordinates detached local process groups, Docker Compose services, trusted direct-argv setup tasks, and scoped environment providers (such as 1Password or external command execution) from a unified dependency graph with zero resident daemon overhead.

---

## Table of Contents

- [Overview & Architecture](#overview--architecture)
- [Installation](#installation)
  - [Homebrew (macOS)](#homebrew-macos)
  - [From Source (Cargo)](#from-source-cargo)
- [Quick Start](#quick-start)
- [Key Features](#key-features)
- [Configuration Reference & Discovery](#configuration-reference--discovery)
  - [Discovery Order](#discovery-order)
  - [Project Registry](#project-registry)
  - [Configuration Example (`hum.yaml`)](#configuration-example-humyaml)
  - [Environment Precedence & Overlays](#environment-precedence--overlays)
- [Runtime Mechanics & Execution Model](#runtime-mechanics--execution-model)
  - [Zero-Daemon Process Runtime](#zero-daemon-process-runtime)
  - [Docker Compose Integration](#docker-compose-integration)
  - [Lifecycle Tasks & Pre-flight Diagnostics](#lifecycle-tasks--pre-flight-diagnostics)
  - [Readiness & Health Checks](#readiness--health-checks)
- [Environment & Secrets Management](#environment--secrets-management)
  - [Supported Providers](#supported-providers)
  - [Security & Isolation Model](#security--isolation-model)
  - [Secrets Caching & Synchronization](#secrets-caching--synchronization)
- [TUI Commands & Keybindings](#tui-commands--keybindings)
  - [Global & Service Navigation](#global--service-navigation)
  - [Log Viewer Controls](#log-viewer-controls)
  - [Modal Dialogs & Quit Confirmation](#modal-dialogs--quit-confirmation)
  - [Status & Health Indicators](#status--health-indicators)
- [CLI Reference & Subcommands](#cli-reference--subcommands)
  - [Syntax & Global Options](#syntax--global-options)
  - [Lifecycle Subcommands](#lifecycle-subcommands)
  - [Management & Configuration Commands](#management--configuration-commands)
  - [Subtractive Filter Options](#subtractive-filter-options)
- [Logging, In-Stream Redaction & Exporters](#logging-in-stream-redaction--exporters)
  - [Sink Architecture & Rotation](#sink-architecture--rotation)
  - [In-Stream Redaction](#in-stream-redaction)
  - [HTTP NDJSON Exporters](#http-ndjson-exporters)
- [Performance & Polling Budget](#performance--polling-budget)
- [Development & Testing](#development--testing)
- [License](#license)

---

## Overview & Architecture

`hum` coordinates complex development environments across disparate technologies without enforcing monolithic project structures:

```text
               ┌──────────────────────────────────────────────┐
               │           hum CLI / Ratatui TUI              │
               │   (Commands, Interactive Monitor & Views)    │
               └──────────────────────┬───────────────────────┘
                                      │
               ┌──────────────────────┴───────────────────────┐
               │        Config Loader & Discovery             │
               │   (hum.yaml, hum.local.yaml, XDG Registry)   │
               └──────────────────────┬───────────────────────┘
                                      │
               ┌──────────────────────┴───────────────────────┐
               │          Core & Graph Resolver               │
               │    (Topological Sort, Cycles, Readiness,     │
               │        Subtractive Exclusions)               │
               └──────────┬───────────────────┬───────────────┘
                          │                   │
         ┌────────────────┴───┐           ┌───┴──────────────────┐
         │  Runtime Adapters  │           │ Scoped Env Providers │
         └────┬───────────┬───┘           └───┬──────────────┬───┘
              │           │                   │              │
    ┌─────────┴─────┐   ┌─┴─────────────┐  ┌──┴───────────┐ ┌┴────────────┐
    │ Process       │   │ Compose       │  │ one-password │ │ exec        │
    │ (Detached     │   │ (Docker       │  │ (1Password   │ │ (Subprocess │
    │  PGID / Lock) │   │  Compose CLI) │  │  CLI / op://)│ │  JSON/Env)  │
    └─────────┬─────┘   └───────────────┘  └──────────────┘ └─────────────┘
              │
    ┌─────────┴───────────────────────────────────────────────┐
    │              Persistent State & Observability           │
    │   - Atomic State Registry: $XDG_STATE_HOME/hum/<project>│
    │   - Native Log Sink: Bounded rotation & regex redaction │
    │   - HTTP Exporter: Non-blocking NDJSON telemetry stream │
    └─────────────────────────────────────────────────────────┘
```

When you execute `start`, `hum` launches each selected unit through its configured runtime, writes minimal metadata into the persistent atomic registry, and exits immediately. Subsequent CLI or TUI invocations observe running services directly from the OS without relying on a background daemon.

---

## Installation

### Homebrew (macOS)

Install the release formula via the official tap:

```bash
brew install delaudio/tap/hum
```

See [`docs/HOMEBREW.md`](docs/HOMEBREW.md) for tap bootstrap and release details.

### From Source (Cargo)

Build and install locally using Cargo:

```bash
git clone https://github.com/delaudio/hum.git
cd hum
cargo install --path .
```

---

## Quick Start

1. **Register a project** from any repository checkout:
   ```bash
   hum project register demo ./hum.yaml
   ```

2. **Inspect the dependency plan** before starting:
   ```bash
   hum demo all-services plan
   ```

3. **Start the stack** (starts units in topological order and exits):
   ```bash
   hum demo all-services start
   ```

4. **Monitor services interactively** in the terminal:
   ```bash
   hum demo all-services tui
   # or simply
   hum demo all-services
   ```

5. **Synchronize provider secrets** (e.g. 1Password) into local cached stores:
   ```bash
   hum demo all-services secrets sync
   ```

6. **Stop services cleanly** in reverse dependency order:
   ```bash
   hum demo all-services stop
   ```

---

## Key Features

### 🚀 Zero-Daemon Architecture & Detached Processes
- **Daemonless Execution**: Commands start processes, record atomic state, and exit. No resident supervisor daemon consumes background CPU or leaks state.
- **Process Group Isolation**: Each service runs in a dedicated session and process group (PGID) with a random, inherited identity lock to avoid signaling reused PIDs.
- **Atomic Rollback**: If a multi-service startup fails midway, only units created by that specific invocation are safely torn down.

### 🕸️ Dependency Graph & Readiness Controls
- **Topological Lifecycle**: Units are started in strict dependency order and stopped in reverse order.
- **Granular Readiness**: Control when dependent units unblock via `depends_on_ready: started | listening | healthy`.
- **Subtractive Exclusions**: Refine service selection on the fly with `--exclude TEMPLATE` and `--exclude-service SERVICE`.

### 🔌 Pluggable Runtimes (Process & Docker Compose)
- **Native Process Adapter**: Detached local processes with working directories, env files, and port monitoring.
- **Docker Compose Adapter**: Maps services to compose targets, manages project profiles, merges generated layers, and supports `reconcile: true` to update running containers when configurations change.

### 🔐 Scoped Environment Providers & Secret Isolation
- **1Password & Exec Providers**: Resolve secrets on demand from `op://` vault references or external commands (`dotenv` or `json` payloads).
- **Process-Level Scoping**: Secrets are injected only into the target child processes—never leaked into the global `hum` environment or written to Compose files.
- **Cached & Schema-Validated**: Plaintext caches are saved with mode `0600` under `.hum/cache/` and checked against explicit key schemas.

### 📜 Bounded Persistent Logging, Redaction & Telemetry
- **Native Log Sink**: Captures stdout and stderr with configurable file rotation (e.g. 10 MiB, 3 files).
- **In-Stream Masking**: Redacts sensitive strings (tokens, keys, passwords) before displaying in CLI/TUI or exporting.
- **NDJSON HTTP Exporters**: Bounded, non-blocking telemetry stream to local or remote collectors.

### 🩺 Diagnostic Doctor & Conflict Detection
- **Pre-Flight Checks**: Verifies CLI dependencies, file existence, port collisions, and provider executables without reading vault secrets.
- **Port Diagnosis**: Differentiates between managed listeners, foreign port owners, and stale registry entries.

### 🎛️ Interactive Terminal UI (Ratatui)
- **Live Monitoring**: Non-blocking background polling with minimal resource usage (< 2% CPU, < 60 MiB RSS).
- **Integrated Log Viewer**: Search (`/`), horizontal panning, paging from disk history, and live follow.
- **Service Details & Actions**: View PID, PGID, port, uptime, health status, and trigger start/stop/restart or open URLs directly.

---

## Configuration Reference & Discovery

### Discovery Order

When a project path is not passed explicitly via `--config`, `hum` resolves configuration in the following order:
1. `./hum.yaml` searching upward through parent directories.
2. `$XDG_CONFIG_HOME/hum/hum.yaml` (defaulting to `~/.config/hum/hum.yaml`).
3. Machine-local overrides in `hum.local.yaml` located beside the resolved `hum.yaml`.

### Versioned Configuration Imports

Large version 3 projects can split committed configuration into explicit YAML
fragments. Declare each fragment once in the main `hum.yaml`:

```yaml
version: 3
project: demo

imports:
  - hum/core.yaml
  - hum/services/api.yaml
  - hum/templates.yaml
```

Imported files use the same top-level sections as `hum.yaml`, but must not
declare `version`, `project`, or another `imports` list. Paths are relative to
the directory containing the main configuration and must stay below it. Hum
loads fragments in declaration order, rejects duplicate paths and duplicate
named repositories, runtimes, providers, services, tasks, templates, or profiles, then
applies the optional machine-local `hum.local.yaml` override and validates the
complete configuration. All runtime paths remain relative to the main
`hum.yaml`, regardless of which fragment declares them.

For example, `hum/services/api.yaml` can colocate the service with its startup
task and focused template:

```yaml
tasks:
  prepare-api:
    command: ["./scripts/prepare-api.sh"]

services:
  api:
    runtime: local
    command: npm run dev
    depends_on: [prepare-api]

templates:
  api:
    services: [api]
```

Keep secrets and machine-specific paths in `hum.local.yaml`; imports are for
portable, versioned project configuration.

### Project Registry

Register projects globally in `~/.config/hum/config.yaml` (or `$XDG_CONFIG_HOME/hum/config.yaml`):

```yaml
version: 1

projects:
  demo:
    config: ~/code/demo/hum.yaml
```

Manage registrations with the CLI:

```bash
hum project register <NAME> <PATH>
```

### Configuration Example (`hum.yaml`)

Here is an annotated, production-ready Version 3 configuration:

```yaml
version: 3
project: sample

# 1. Define runtime adapters
runtimes:
  local:
    type: process
  containers:
    type: compose
    project_name: hum-sample
    reconcile: true
    files:
      - docker-compose.yml

# 2. Configure scoped environment providers
environment_providers:
  team-vault:
    type: one-password
  generic-exec:
    type: exec
    command:
      - echo
      - '{"PORT":"8080"}'

# 3. Define trusted one-shot tasks
tasks:
  migrate:
    command:
      - docker
      - compose
      - --project-name
      - hum-sample
      - --file
      - docker-compose.yml
      - run
      - --rm
      - migrate
    depends_on:
      - database
    timeout: 2m

# 4. Define persistent services
services:
  database:
    runtime: containers
    target: database

  api:
    runtime: local
    command: python3 -m http.server 8080
    port: 8080
    url: http://localhost:8080
    env_file: api.env
    env_from:
      - provider: team-vault
        reference: op://Development/sample-api/environment
        format: dotenv
        optional: true
        schema: api.env.example
        cache: .hum/cache/api.env
      - provider: generic-exec
        args:
          - --json
        format: json
        optional: true
    depends_on:
      - migrate
    depends_on_ready: healthy
    healthcheck:
      type: http
      url: http://localhost:8080/health
      interval: 2s
      timeout: 1s
      retries: 15

# 5. Define selectable templates
templates:
  backend:
    services:
      - api
  infrastructure:
    services:
      - database
  all-services:
    services:
      - database
      - api
```

### Environment Precedence & Overlays

Environment variables for a service are resolved using the following strict precedence hierarchy (highest to lowest):

1. **CLI `--env KEY=VALUE`** explicit overrides (repeatable).
2. **Inherited environment** from the launching shell.
3. **`service.env_overrides`** (often sourced from `hum.local.yaml` for host vs container routing).
4. **Provider-backed values** (`env_from` via 1Password / Exec).
5. **`service.env`** declared in `hum.yaml`.
6. **`env_file`** loaded from disk.

---

## Runtime Mechanics & Execution Model

### Zero-Daemon Process Runtime

- **Process Isolation**: Each process is launched in its own process group (`setsid` / `setpgid`) with stdin redirected to `/dev/null` and stdout/stderr connected to the rotating log sink.
- **State Persistence**: State is serialized atomically into `$XDG_STATE_HOME/hum/<project>/` (or `~/.local/state/hum/<project>/`). It records PID, PGID, start time, working directory, port, and log file paths.
- **PID Verification**: Before sending any signal (`SIGTERM`, `SIGKILL`), `hum` validates the process start time against OS process tables to eliminate PID reuse hazards.
- **Concurrency & Locking**: File-based `project.lock` serializes concurrent operations, guaranteeing that duplicate or racing `start` invocations are safe and idempotent.

### Docker Compose Integration

- **Target Mapping**: Maps hum service names to Compose service targets.
- **Dynamic Layers**: Project tasks can emit `generated_files` (e.g. host networking configurations) which Compose merges automatically once created.
- **Reconciliation**: Runtimes with `reconcile: true` reapply running containers when provider values or generated overlays change.
- **Clean Teardown vs Reset**: `stop` preserves named volumes; `reset` deletes Compose volumes and requires explicit confirmation or the `--yes` flag.

### Lifecycle Tasks & Pre-flight Diagnostics

- **Direct Argv**: Tasks execute direct argument vectors (`["docker", "compose", "run", ...]`) without shell string interpolation vulnerabilities.
- **Idempotency Checks**: Tasks can define an optional `check` command to avoid rerunning completed tasks.
- **Doctor Diagnostic Sub-commands**: Tasks can define a read-only `doctor` check executed during `hum doctor` to validate product prerequisites without exposing secret environments.

### Readiness & Health Checks

Control sequencing with `depends_on_ready`:

| Readiness Mode | Unblock Condition |
| :--- | :--- |
| `started` | Unblocks immediately once the process or container is launched. |
| `listening` | Unblocks once the configured TCP port actively accepts connections (50 ms connect deadline). |
| `healthy` | Unblocks once HTTP or TCP health check probes succeed for the configured retry threshold. |

---

## Environment & Secrets Management

### Supported Providers

1. **`one-password`**: Resolves `op://vault/item/field` or whole-document dotenv secrets using the 1Password CLI (`op`).
2. **`exec`**: Runs an arbitrary command returning key-value pairs formatted as `dotenv` or `json`.

### Security & Isolation Model

- **Least Privilege**: Secret values are passed exclusively through child process environment tables or Compose invocations. They are never exported to your shell or stored in world-readable temporary files.
- **Safe Compose Inspection**: Inspect effective configurations safely with `hum <project> <template> config compose --format yaml`; all secrets are replaced with `<redacted>` tokens.

### Secrets Caching & Synchronization

- **Encrypted/Restricted Caches**: Cached secret files are stored with `0600` permissions under `.hum/cache/` (which should be added to `.gitignore`).
- **Fail-Closed Semantics**: If a required provider fails and no valid cache exists, startup fails immediately. Optional sources continue gracefully.
- **Manual Sync**: Refresh cached secrets without starting services using:
  ```bash
  hum <project> <template> secrets sync
  ```

---

## TUI Commands & Keybindings

Launch the interactive monitor with `hum <project> <template> tui` (or `hum <project> <template>`):

### Global & Service Navigation

| Key / Shortcut | Action |
| :--- | :--- |
| `Up` / `k` | Move cursor up in service list |
| `Down` / `j` | Move cursor down in service list |
| `Space` | Toggle start / stop on selected service |
| `r` | Restart selected service |
| `Enter` | Open service details modal (PID, PGID, port, health, paths) |
| `l` | Open persistent log viewer for selected service |
| `o` | Open service URL in default web browser |
| `p` | Open template switcher modal |
| `d` | Run doctor pre-flight diagnostics in the background |
| `?` | Open keybindings help overlay |
| `q` | Open quit confirmation dialog |

### Log Viewer Controls

| Key / Shortcut | Action |
| :--- | :--- |
| `Up` / `k` | Scroll logs up by 1 line |
| `Down` / `j` | Scroll logs down by 1 line |
| `PageUp` / `PageDown` | Scroll logs up / down by a page (20 lines) |
| `Home` | Scroll to oldest available log history on disk |
| `End` | Return to bottom and resume live follow mode |
| `Left` / `h` | Scroll log view horizontally left |
| `Right` / `l` | Scroll log view horizontally right |
| `0` | Reset horizontal scroll to beginning |
| `/` | Enter search query mode |
| `c` | Clear log buffer view |
| `Esc` / `q` | Close log viewer |

### Modal Dialogs & Quit Confirmation

| Key / Shortcut | Action |
| :--- | :--- |
| `Esc` | Close any active modal dialog (details, doctor, help, template) |
| `l` (in quit dialog) | **Leave services running** and quit TUI |
| `s` (in quit dialog) | **Stop selected template** and quit TUI |

### Status & Health Indicators

| Indicator | Process State | Health State | Description |
| :--- | :--- | :--- | :--- |
| `●` (Green) | `running` | `healthy` | Process is active and passing health checks. |
| `◐` (Yellow) | `starting` / `stopping` | `checking` | State transition in progress or health check pending. |
| `✗` (Red) | `exited` | `unhealthy` | Process has exited or failed health checks. |
| `○` (Gray) | `missing` | `unchecked` | Not running / no health checks configured. |

---

## CLI Reference & Subcommands

### Syntax & Global Options

```bash
hum [OPTIONS] <PROJECT> <TEMPLATE> [COMMAND]
hum [OPTIONS] project register <NAME> <CONFIG>
```

#### Global Options:
- `--registry PATH`: Override global registry path (default: `~/.config/hum/config.yaml`).
- `--config PATH`: Explicit project `hum.yaml` path (bypasses registry).
- `--env KEY=VALUE`: Override service environment variables (repeatable).

### Lifecycle Subcommands

| Command | Description |
| :--- | :--- |
| `start [service...]` | Start the selected template or listed services in dependency order. |
| `stop [service...] [--timeout 10s]` | Stop services in reverse dependency order. |
| `restart [service...] [--timeout 10s]` | Restart services with a clean stop-start sequence. |
| `status` | Show status, PID, port, and health check state for template services. |
| `plan [service...] [--json]` | Preview resolved dependency order and actions without executing. |
| `logs [service] [-n 100] [-f]` | Tail captured stdout/stderr logs for a service or template. |
| `reset [--yes] [--timeout 10s]` | Stop all project services and purge Compose volumes. |
| `doctor` | Run diagnostic pre-flight checks on ports, tools, and configs. |
| `tui` | Launch the interactive full-screen terminal monitor. |

### Management & Configuration Commands

```bash
# Register a project into the machine-local registry
hum project register <NAME> <CONFIG_PATH>

# Validate configuration syntax and template selection
hum <project> <template> config validate

# Render effective Docker Compose configuration with redacted secrets
hum <project> <template> config compose [--format yaml|json] [--runtime NAME]

# Synchronize provider-backed secrets into local 0600 cache files
hum <project> <template> secrets sync [service...]
```

### Subtractive Filter Options

Refine startup and diagnostics on the fly:
- `--exclude TEMPLATE`: Remove root services from that template. Reintroduced automatically with a warning if needed by a dependent unit.
- `--exclude-service SERVICE`: Strictly exclude a service; fails before starting if it remains a required dependency.

---

## Logging, In-Stream Redaction & Exporters

### Sink Architecture & Rotation

For native detached processes, `hum start` connects output pipes to a dedicated native log sink (`hum __log-sink`).

```yaml
logs:
  max_file_bytes: 10485760   # 10 MiB per log file
  rotated_files: 3           # Keep 3 rotated archives (.1, .2, .3)
  max_line_bytes: 65536      # 64 KiB buffer ceiling
  retention: 7d              # Remove rotated logs older than 7 days
```

- Raw files on disk remain byte-accurate.
- Closing CLI/TUI log viewers never delivers `SIGPIPE` to running services.

### In-Stream Redaction

Define regular expressions in `hum.yaml` to redact sensitive credentials before they reach the terminal or telemetry exporters:

```yaml
logs:
  redact_patterns:
    - "(?i)(token|password|secret)=[^ ]+"
    - "bearer [a-zA-Z0-9_\\-\\.]+"
```

### HTTP NDJSON Exporters

Stream log events asynchronously to an observability collector:

```yaml
logs:
  exporters:
    - type: http
      endpoint: http://127.0.0.1:8687/events
      timeout: 750ms
      headers:
        Authorization: "Bearer machine-local-token"
```

- Bounded non-blocking queue: Collector downtime never slows or blocks services.
- Secret headers are passed via private Unix domain socket descriptors and never written to temporary files.

See [`docs/LOGGING.md`](docs/LOGGING.md) for full logging mechanics and configuration contracts.

---

## Performance & Polling Budget

`hum` is engineered for negligible overhead in long-running development sessions:

- **Startup Latency**: 10 detached services start in < 2 seconds.
- **TUI Frame Latency**: First frame renders in < 250 ms.
- **Resource Footprint**: Steady-state monitor consumes < 2% average CPU and < 60 MiB RSS memory for 10 monitored services.
- **Non-Blocking Polling**: Port checks use 50 ms TCP timeouts; `lsof` is never executed during steady-state polling.

See [`docs/POLLING.md`](docs/POLLING.md) for polling contract details and benchmarking procedures.

---

## Development & Testing

Run the full code quality and testing suite locally:

```bash
# Code formatting & clippy lints
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings

# Execute test suite with minimum test count gate
scripts/verify-test-count.sh 60

# Build optimized release binary
cargo build --release

# Run performance smoke benchmarks
cargo test --release --test performance_smoke -- --ignored --nocapture

# Release validation
scripts/release-guard.sh v0.6.2
```

For complete product specifications and acceptance criteria, see [`docs/PRD.md`](docs/PRD.md).

---

## License

This project is licensed under the [MIT License](LICENSE).
