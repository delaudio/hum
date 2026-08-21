# PRD — hum: local development process launcher and monitor

**Product Name:** `hum`

**Tagline:** Keep your local stack humming.

**Type:** CLI and TUI for orchestrating local development runtimes

**Stack:** Rust, Ratatui, Tokio

**Status:** Draft

**Product Version:** 0.6.1

**Project Configuration Version:** 3

> Version 3 extends without invalidating process-only v1/v2 files. Product integrations belong in configuration packs external to the `hum` repository.

## 1. Summary

`hum` starts, stops, monitors, and diagnoses local services through generic adapters. The graph can include processes, Docker Compose services, and declarative one-shot tasks.

The main flow is explicit:

```bash
hum <project> <template> <command>
```

For example:

```bash
hum demo all-services start
```

`start` launches selected services via their respective runtime and then exits. No resident `hum` daemon remains. Subsequent CLI or TUI invocations discover running services through a persistent runtime registry and verify their state with the operating system.

The TUI is a runtime observer and controller, not the owner of service lifetimes. Opening or closing it does not implicitly start or stop processes.

## 2. Problem

Local development of a multi-service product requires remembering:

- repository checkout paths and working directories;
- commands and environment variables;
- services required for a specific task;
- dependency order;
- ports and health checks;
- PIDs, status, and logs of launched processes;
- shutdown and recovery procedures after a crash.

An orchestrator that retains everything only in session memory cannot support separate commands like `start`, `status`, `logs`, and `stop`. It also forces the TUI to remain open and couples services to its output pipes.

`hum` solves this problem by keeping configuration declarative and persisting only minimal runtime metadata. Application processes remain standard local processes, inspectable even without `hum`.

## 3. Concepts

### 3.1 Project

A registered local product with a stable name, for example `demo`. The global registry maps the name to the project configuration file.

### 3.2 Runtime

A named adapter configured by the project. Version 3 includes `process` and `compose`; no adapter contains product names, images, or policies.

### 3.3 Service

A launchable and monitorable process: frontend, API, worker, Storybook, mock server, or a database started via a local command.

The runtime identity is `<project>/<service>`. The same service included in two templates is never started twice.

### 3.4 Task

A trusted-code one-shot unit executed as a direct argv, with timeout, dependencies, and optional idempotency check. It is not a persistent service.

### 3.5 Environment provider

A named provider that resolves environment sources for the target child process only. Supported adapters include `one-password` (via 1Password CLI) and `exec` (via a generic command returning `dotenv` or `json` payloads), without ever exporting values into `hum`'s global process.

### 3.6 Template

A named selection of services for a workflow context, for example `frontend`, `backend`, or `all-services`. Replaces the v1 concept of `profile`.

A template selects services; it does not own processes. Stopping a template requests stopping its selected services in reverse dependency order.

### 3.7 Runtime registry

Persistent metadata allowing separate invocations to recognize a process:

- project and service;
- PID and process group ID;
- process start time or equivalent identifier;
- command/config hash;
- working directory;
- launch timestamp;
- expected port;
- log file paths.

The PID alone is not sufficient identity: before sending a signal, `hum` verifies start time and available metadata to avoid signaling a reused PID.

### 3.8 Process, port, and health

These are distinct signals:

```text
Process  starting | running | exited | missing | stopping
Port     listening | closed | unknown | occupied-by-other
Health   unchecked | checking | healthy | unhealthy
```

The UI may derive a summary label, but must retain the three underlying values. A process can be `running`, have its port `listening`, and be `unhealthy` simultaneously.

## 4. Goals

- Start a local environment with a single non-interactive command.
- Keep services running after CLI and TUI exit.
- Make `status`, `stop`, `restart`, and `logs` work across independent invocations.
- Explicitly select project and template.
- Manage acyclic dependencies and configurable readiness.
- Retain persistent logs with bounded disk usage.
- Monitor PID, ports, and health with lightweight polling.
- Diagnose configuration and stale runtime state.
- Consume negligible resources relative to managed services.
- Initially support macOS and Linux.
- Manage Compose without replacing it or duplicating its model.
- Obtain environments from declarative providers without printing sensitive values.
- Keep all product-specific knowledge in external configuration packs.

## 5. Non-Goals

Initial releases must not:

- introduce a resident central `hum` daemon;
- automatically restart services after a crash;
- replace Docker Compose, Kubernetes, launchd, or systemd;
- manage deployments, remote hosts, or replicas;
- include a REST API, authentication, or plugins;
- download and execute remote configurations automatically;
- act as a secret vault or store secrets in its own schema;
- become a package manager or job scheduler.

Services run in the background, but this independence does not imply a supervisor process.

## 6. CLI

### 6.1 Grammar

```text
hum <project> <template> <command> [arguments]
```

Commands:

```bash
hum demo all-services start
hum demo all-services stop
hum demo all-services restart
hum demo all-services status
hum demo all-services plan --json
hum demo all-services secrets sync
hum demo all-services config compose --format yaml
hum demo all-services logs api --follow
hum demo all-services reset
hum demo all-services doctor
hum demo all-services tui
```

`plan`, `start`, `doctor`, and `secrets sync` accept repeatable exclusions:

```bash
hum demo all-services plan --exclude identity
hum demo all-services start --exclude-service mail
```

`--exclude` subtracts root services of the specified template, but a remaining unit can reintroduce a dependency with a deterministic warning. `--exclude-service` is a strict veto: if a remaining unit depends on the excluded service, selection fails before any side effect.

`plan --json` returns a stable machine-readable document for resolved plans containing template, required roots, exclusions, warnings, and ordered units with inclusion rationale. An unresolvable plan exits with a non-zero exit code and diagnostic stderr. `config compose` renders the effective Compose model without contacting providers, replacing service environment values with `<redacted>`.

Interactive entry points:

- `hum` opens project and template selection;
- `hum <project>` opens template selection for the project;
- `hum <project> <template>` opens the TUI in the selected context;
- `tui` explicitly invokes the TUI for scripts and documentation.

### 6.2 Semantics

`start`:

1. loads registry and configuration;
2. resolves template and dependencies;
3. acquires a project lock;
4. reconciles existing persisted state;
5. starts missing services in independent process groups/sessions;
6. redirects stdin, stdout, and stderr before returning;
7. persists runtime metadata atomically;
8. exits without leaving a resident `hum` process.

`stop` sends `SIGTERM` to the process group, waits for the configured timeout, and uses `SIGKILL` only if necessary. It verifies process identity before sending signals and stops services in reverse dependency order.

`restart` operates on the registered process and is not an uncoordinated start.

`status` reconciles registry and operating system. It does not treat registry entries alone as proof that a process is alive.

`logs` reads persistent files and supports tailing and live follow.

### 6.3 Exit Codes

```text
0   operation completed successfully
1   generic error
2   invalid configuration
3   project not found
4   template not found
5   service not found
6   start failed or partial
7   stop failed or partial
8   health/readiness failed
9   doctor failed
10  runtime registry incoherent
```

`already-running` and `already-stopped` exit codes are `0` when the requested outcome is already satisfied and process identity is verified.

## 7. Configuration

### 7.1 Global Registry

Path:

```text
$XDG_CONFIG_HOME/hum/config.yaml
~/.config/hum/config.yaml              # fallback
```

Example:

```yaml
version: 1

projects:
  demo:
    config: ~/code/demo/hum.yaml
```

### 7.2 Project Configuration

Shared file `hum.yaml`, with optional committed YAML fragments declared by an
explicit `imports` list and an optional untracked override `hum.local.yaml`.
Version 3 adds named runtimes, environment providers, tasks, and readiness
controls:

```yaml
version: 3
project: demo

imports:
  - hum/additional-services.yaml

runtimes:
  local:
    type: process
  containers:
    type: compose
    project_name: demo-local
    files: [compose.yaml]
    reconcile: true

environment_providers:
  development:
    type: one-password
  script:
    type: exec
    command: ["./scripts/get-env.sh"]

switch_provider:
  command: ["./scripts/runtime-switch"]

repositories:
  applications:
    path: ./demo-applications
  api:
    runtime: local
    path: ./demo-api

services:
  api:
    repository: api
    command: pnpm dev
    port: 3000
    env_file: .env
    env_from:
      - provider: script
        args: ["api"]
        format: json
        optional: true
    healthcheck:
      type: http
      url: http://localhost:3000/health

  database:
    runtime: containers
    target: postgres

  frontend:
    repository: applications
    cwd: apps/procurement-frontend
    command: pnpm dev
    port: 5173
    depends_on:
      - api
      - database
    depends_on_ready: healthy

templates:
  all-services:
    services:
      - api
      - frontend
```

Import paths must be unique, relative, and remain below the directory containing
the main `hum.yaml`. Fragments may use the normal configuration sections but do
not redeclare `version`, `project`, or `imports`. Named definitions may appear
in exactly one committed file. Hum merges the fragments in declaration order,
applies `hum.local.yaml` last, and validates the complete configuration.

All relative runtime paths resolve from the directory containing the main
`hum.yaml`, not from the fragment or the working directory where `hum` is
executed.

Precedence order:

```text
defaults
  < composed versioned config (hum.yaml + imports)
  < hum.local.yaml
  < environment
  < CLI arguments
```

Unknown schema fields are rejected. `env_file` values take lower precedence than `service.env`, process environment, and CLI overrides. Errors include file, field, position, and action hints.

Compose runtimes can declare `reconcile: true` to reapply `compose up` when provider environment variables or generated layers change.

An `environment_provider` supports both `type: one-password` (with optional `account`) and `type: exec` (with executable `command: [...]`). `env_from` entries can specify `format: dotenv` (default) or `format: json` (for flat JSON string maps with keys containing `/` or `:`), per-source `args: [...]`, `schema`, `cache`, and `optional`.

An optional `switch_provider` exposes product-defined runtime modes through
`hum PROJECT TEMPLATE switch MODE [SERVICE...]`. Hum executes its direct argv
from the project root, appending the selected mode, service names, `--all`,
`--template NAME`, and `--no-start` when requested. The provider owns checkout,
routing, persistence, and transition policy; Hum validates named services and
requires them to belong to the selected template's resolved dependency set,
then propagates provider failure through a stable non-zero exit code. Provider argv
is trusted project configuration: multi-component executable paths resolve from
the project root, while absolute paths and parent traversal remain supported for
adapters shared by multiple adjacent projects.

A mode may intentionally receive no service names, for example to render
adapter-owned status. Hum forwards that empty selection without assigning
product-specific meaning; mutating modes define whether names or `--all` are
required.

Services can define `depends_on_ready: started | listening | healthy` to control when dependent units in the graph are unblocked.

## 8. Persistent State and Locking

Default directory:

```text
$XDG_STATE_HOME/hum/<project>/
~/.local/state/hum/<project>/          # fallback
```

Layout:

```text
~/.local/state/hum/demo/
├── runtime/
│   ├── api.json
│   └── frontend.json
├── logs/
│   ├── api.stdout.log
│   ├── api.stderr.log
│   └── frontend.stdout.log
└── project.lock
```

Registry writes are atomic: temporary file in the same directory, flush, and rename. `project.lock` serializes concurrent operations on the same project.

### 8.1 Stale Reconciliation

An entry is stale when the process does not exist or its identity does not match. `status` displays it explicitly; `doctor` explains the root cause. Cleanup occurs automatically only after verifying that no process could receive signals by mistake.

A process found on an expected port but unverified in the registry is reported as `occupied-by-other`, not a managed service.

## 9. Process Management

Each service uses a dedicated process group. On macOS and Linux it is detached from terminal and `hum` session. stdin is connected to null; stdout and stderr are redirected to files before `start` returns.

Configured shell commands are treated as trusted code. Configurations are never downloaded from remote sources automatically.

`start` is idempotent. Before spawning a process it checks registry, PID identity, and port. A conflict does not launch a duplicate process.

Service crash behavior:

- does not restart a process automatically;
- remains visible via status, exit code when available, and logs;
- does not crash CLI or TUI;
- can be recovered with `restart`.

## 10. Polling and Health

Target polling intervals:

- PID existence/identity: 500–1000 ms in TUI;
- TCP port check: 1–2 s or configured interval;
- HTTP/TCP health probe: service-configured interval.

PID checks are targeted. Port checks use non-blocking short TCP connections. HTTP clients are pooled.

`lsof` is used only to identify port occupants during explicit diagnostics or conflicts. It is never executed in steady-state rendering loops.

Probes are cancellable, non-overlapping, and generation-bound. Previous generation results cannot overwrite state after a restart.

## 11. Logs

Logs are persistent and separate stdout and stderr. Every displayed line includes timestamp, service name, stream, and content.

Retention is bounded by byte size and file count. Configurable options:

- maximum file size;
- rotated file count;
- maximum line/chunk bytes;
- regex redaction patterns for display and export.

The TUI reads disk files incrementally without loading full log history into memory. `logs --follow` continues streaming independently of the TUI.

`logs.exporters` can forward process-runtime log copies as NDJSON over HTTP to local collectors (such as Vector), storing authentication headers in `hum.local.yaml` without blocking service execution.

## 12. TUI

The TUI displays the selected project and template for every service:

- process state and PID/PGID;
- uptime;
- port and port state;
- health state and last result;
- exit code or error;
- persistent log access.

Initial shortcuts:

```text
↑/k, ↓/j   navigation
space      explicit start/stop
r          restart
enter      details
l          logs
d          doctor
o          open URL
?          help
q          close TUI without stopping services
```

Slow operations and `doctor` run off the main event loop. Action errors are reported explicitly. A full stack shutdown requires explicit confirmation; quitting the TUI leaves services running.

## 13. Doctor

`doctor` checks:

- global registry and project configuration;
- repositories, working directories, and required commands;
- required files and env files;
- environment variables without printing secret values;
- dependencies and cycle detection;
- stale runtime registry entries;
- registered process identities;
- closed, managed, or foreign port occupants;
- state/log directory permissions.

A port occupied by the expected `hum` service is valid. A port occupied by an unrecognized process is diagnosed distinctly.

## 14. Non-Functional Requirements

- Single binary distribution.
- TUI visible within ~200 ms with valid local configuration.
- Negligible CPU and RSS overhead with ten managed services.
- RAM usage independent of log history length.
- No full process table scanning during steady-state polling.
- Service crash or high log volume does not crash `hum`.
- Initial macOS and Linux support.
- Error messages with actionable hints.
- Simple, readable, versionable configuration files.

## 15. Security

- Verify identity and start time before signaling registered PIDs.
- Use owner-only permissions (`0600`/`0700`) for state, logs, and caches.
- Never print sensitive environment variable values.
- Never write sensitive values into generated Compose files.
- Write provider caches atomically with `0600` permissions.
- Fail closed when required providers and schema-valid caches are missing.
- Never launch interactive login prompts during `start`; `secrets sync` prompts only when attached to a terminal.
- `doctor` verifies provider availability without reading vault secrets.
- Redact configured sensitive regex patterns in display and export views.
- Treat local configuration commands as trusted code.
- Do not modify project `.env` files; provider caches use distinct, gitignored paths.

## 16. Configuration Migration

Version 1 used `profiles` and implicit discovery. Version 2 migration steps:

1. register project in global registry;
2. add `project` key to project file;
3. change `version: 1` to `version: 2`;
4. rename `profiles` to `templates`;
5. replace `hum up <profile>` with `hum <project> <template> start`.

Version 1 files produce an actionable migration error.

Version 3 retains v1/v2 process services while adding named runtimes, Compose integration, tasks, environment providers, and readiness controls.

## 17. Acceptance Criteria

### CLI and Configuration

- `hum demo all-services start` works from any directory.
- Unknown project or template errors are distinct and actionable.
- Overlapping templates do not duplicate services.
- Graph orders processes, Compose services, and tasks without cycles.
- `plan` does not contact Docker or providers and redacts secret values.
- `plan --json` exposes selection, exclusions, warnings, and rationale in machine-readable format.
- `--exclude` warns on reintroduced dependencies; `--exclude-service` blocks execution before side effects.
- `switch` forwards additive named selections or `--all` to the configured project adapter without shell parsing.

### Lifecycle

- No resident daemon process remains after `start`.
- Subsequent CLI/TUI invocations discover and manage running services.
- `stop` terminates the full process group safely without signaling reused PIDs.
- Stale entries and partial failures are visible and recoverable.

### Observability

- Process, port, and health signals remain distinct.
- TUI observes active processes without owning their lifetime.
- Logs and status remain available after TUI exits.
- Steady-state polling avoids `lsof` or full process table scans.

### Quality

- Integration tests cover multi-invocation lifecycle with real processes.
- `cargo fmt --check`, Clippy, unit/integration tests, and release builds pass cleanly.
- Tests leave no residue.
- Docker fake test verifies project isolation, start/stop/status/logs, reset, and secret redaction.

## 18. Proposed Release Criteria

The release is ready when a developer can configure Demo and run:

```bash
hum demo all-services doctor
hum demo all-services start
hum demo all-services status
hum demo all-services tui
```

Closing the TUI leaves services running. Later:

```bash
hum demo all-services logs api --follow
hum demo all-services stop
```

The same model applies to additional projects by adding a single entry to the global registry and a versioned `hum.yaml`.
