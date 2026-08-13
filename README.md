# hum

`hum` is a product-neutral launcher and terminal monitor for local
multi-service development environments. Version 3 can coordinate detached
local processes, Docker Compose services, direct-argv setup tasks, and scoped
environment sources such as 1Password from one dependency graph.

Current project/template CLI:

```bash
hum demo all-services start
```

The command starts each selected unit through its configured runtime and exits;
there is no resident `hum` daemon. Product names, images, Compose projects,
vault references, migrations, and bootstrap commands belong in the project's
configuration pack, not in the `hum` binary.

## Installation

Install the release formula directly from the public Homebrew tap:

```bash
brew install delaudio/tap/hum
```

This installs a native binary on Apple Silicon or Intel macOS without requiring
the Rust toolchain. Release operations and the one-time tap bootstrap are
documented in [`docs/HOMEBREW.md`](docs/HOMEBREW.md).

## Concepts

- **Project**: a registered local product, such as `demo`.
- **Template**: a named service selection, such as `frontend` or
  `all-services`.
- **Runtime**: a named generic adapter (`process` or `compose`).
- **Service**: one runtime-owned long-lived unit.
- **Task**: a trusted, direct-argv one-shot unit in the same dependency graph.
- **Environment provider**: a named source of scoped values; currently
  `one-password` with dotenv payloads.

## Target CLI

```bash
hum <project> <template> start [service ...] [--exclude TEMPLATE] [--exclude-service SERVICE]
hum <project> <template> stop
hum <project> <template> restart
hum <project> <template> status
hum <project> <template> plan [service ...] [--json] [--exclude TEMPLATE] [--exclude-service SERVICE]
hum <project> <template> secrets sync [service ...] [--exclude TEMPLATE] [--exclude-service SERVICE]
hum <project> <template> logs [service] [--lines N] [--follow]
hum <project> <template> reset [--yes]
hum <project> <template> doctor [--exclude TEMPLATE] [--exclude-service SERVICE]
hum <project> <template> config compose [--format yaml|json] [--runtime NAME]
hum <project> <template> tui
```

`hum`, `hum <project>`, and `hum <project> <template>` provide progressively
narrower interactive selection, with the final form opening the TUI.

## Configuration discovery

The binary accepts legacy v1/v2 process configurations and the v3 adapter
format. It discovers
`hum.yaml` from the current directory upwards, then at
`$XDG_CONFIG_HOME/hum/hum.yaml`, falling back to
`~/.config/hum/hum.yaml`. The runnable example is
[`examples/hum.example.yaml`](examples/hum.example.yaml).

Register a project from any checkout path. Hum validates the configuration and
stores its canonical path in the machine-local registry at
`~/.config/hum/config.yaml` (or `$XDG_CONFIG_HOME/hum/config.yaml`):

```bash
hum project register demo ./hum.yaml
```

The resulting registry uses the v1 registry contract:

```yaml
version: 1

projects:
  demo:
    config: ~/code/demo/hum.yaml
```

A copyable registry shape is available at
[`examples/registry.example.yaml`](examples/registry.example.yaml).

Each project owns a versioned `hum.yaml`. See the process-only
[`examples/hum.v2.example.yaml`](examples/hum.v2.example.yaml) and the v3
[`examples/hum.v3.example.yaml`](examples/hum.v3.example.yaml). Machine-specific
values can live in an untracked `hum.local.yaml` beside it.

For services that switch between container and host execution, optional
`env_overrides` values are applied after provider-backed dotenv values. This
lets a machine-local overlay project dependency URLs to `localhost` without
editing or duplicating secrets; inherited variables and explicit CLI `--env`
values still have the final precedence.

Relative paths are resolved from the configuration file that declares them.
Repository paths are relative to `hum.yaml`; service `cwd` is relative to its
repository, and `env_file` is relative to the resulting service working directory.
Service environment precedence is `.env` file, `service.env`, provider sources,
`service.env_overrides`, inherited process environment, then repeatable
`--env KEY=VALUE` CLI overrides. Hum warns with key names only when declared
overrides replace provider values. Unknown YAML fields and invalid names, ports,
URLs, durations, commands, or references are rejected.

## Version 3 adapters and providers

A v3 service selects a named runtime. Process services retain the detached
registry described below. Compose services map a hum name to a runtime-native
`target`; lifecycle and logs use `docker compose` with the configured project
name, files, profiles, and env file. Project tasks may produce optional
`generated_files` layers (for example host-specific network mappings); Compose
includes each layer only after it exists, while `doctor` reports it as a
generated artifact. A Compose runtime may opt into `reconcile: true` to reapply
selected running targets when provider values or generated layers change; the
default preserves the start-only behavior. Reapplied services are reported as
`reconciled`, separately from untouched `already running` services. `stop`
preserves volumes; `reset` is the
only volume-deleting operation and requires the project name interactively or
`--yes` for automation.

Tasks use an argv array and are never wrapped in an implicit shell. An optional
`check` argv makes a task idempotent. An optional read-only `doctor` argv runs
only during diagnostics, without provider-backed values, so product packs can
check their own configuration and prerequisites without teaching Hum about the
product. Tasks and services share dependency and cycle validation; a failed
task rolls back only services started by the current invocation. `plan` resolves
this graph without contacting providers or Docker.

Selections support repeatable subtractive filters. `--exclude TEMPLATE`
removes that template's services from the initial roots; if a remaining unit
still depends on one, it is reintroduced with a warning naming the consumer.
`--exclude-service SERVICE` is strict: if the service is still required, the
command fails before Docker, tasks, or providers are contacted. The same
resolver is used by `plan`, `start`, `doctor`, and `secrets sync`.

An `env_from` entry can read a dotenv item from a named 1Password provider.
Values are scoped to the child process or Compose invocation and are never
printed or persisted in generated Compose YAML. Optional `schema` validation
requires an exact key set. Optional `cache` files are plaintext, atomically
replaced with mode `0600`, and should live under the gitignored `.hum/`
directory. Required sources fail closed only when neither the provider nor a
schema-valid cache is available; optional sources may continue empty. Provider
references are read once per lifecycle action and reused across tasks/services;
each later TUI action starts with a fresh provider-read cache.
`secrets sync` refreshes selected sources without starting services and may
attempt interactive `op signin`; normal startup never prompts. `doctor` checks
only that `op` exists—it never reads vault items.

`config compose` renders the effective Compose model without contacting
environment providers. Service environment values are replaced with
`<redacted>` in both YAML and JSON output, while interpolation outside service
environment maps remains as `${NAME}` placeholders.

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
