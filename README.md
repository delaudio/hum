# hum

`hum` is a lightweight launcher and terminal monitor for local multi-service
development environments.

Current v1 CLI:

```bash
hum up full
```

Target v2 CLI (not implemented yet):

```bash
hum compri all-services start
```

The command starts the selected services in independent process groups, writes
their logs and runtime metadata to disk, and exits. There is no resident `hum`
daemon. Later CLI invocations and the TUI reconcile that metadata with the
operating system.

> The repository is currently migrating from the session-owned v1 prototype to
> this v2 model. Track progress in
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
hum <project> <template> logs [service] [--follow]
hum <project> <template> doctor
hum <project> <template> tui
```

`hum`, `hum <project>`, and `hum <project> <template>` provide progressively
narrower interactive selection, with the final form opening the TUI.

## Current configuration (v1)

The binary currently accepts the session-owned v1 format and discovers
`hum.yaml` from the current directory upwards, then at
`$XDG_CONFIG_HOME/hum/hum.yaml`, falling back to
`~/.config/hum/hum.yaml`. The runnable example is
[`examples/hum.example.yaml`](examples/hum.example.yaml).

The current commands remain `hum up <profile>`, `hum status`, and the other v1
subcommands until the linked migration issues land.

## Target configuration (v2, not implemented yet)

The target v2 contract registers projects globally in
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

Each project owns a versioned `hum.yaml`. The future-format example is
[`examples/hum.v2.example.yaml`](examples/hum.v2.example.yaml). It intentionally
does not validate with the current v1 binary yet. Machine-specific values can
live in an untracked `hum.local.yaml` beside it.

Relative paths are resolved from the configuration file that declares them.

## Current runtime and logs (v1)

The v1 process and log state lives only in the running `hum` session. Closing
that session removes the in-memory state, and a later `hum logs` invocation
cannot attach to it. This limitation is the reason for the v2 migration.

## Target runtime and logs (v2, not implemented yet)

State is stored below `$XDG_STATE_HOME/hum/<project>` or
`~/.local/state/hum/<project>`. The registry records PID, process group, process
start time, command identity, working directory, port, and log paths. PID
identity is verified before signals are sent.

The TUI polls known PIDs directly, probes configured ports and health endpoints,
and tails log files incrementally. `lsof` is reserved for diagnosing unknown
port occupants; it is not used for ordinary polling.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

The detailed product contract, lifecycle, security model, migration notes, and
acceptance criteria are in [`docs/PRD.md`](docs/PRD.md).
