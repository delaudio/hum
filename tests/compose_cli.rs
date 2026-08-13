#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn compose_runtime_start_status_and_stop_use_isolated_project_state() {
    let root = std::env::temp_dir().join(format!(
        "hum-compose-cli-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("bin")).unwrap();
    let _cleanup = Cleanup(root.clone());
    let config = root.join("hum.yaml");
    fs::write(
        root.join("compose.yaml"),
        "services:\n  database:\n    image: postgres:17\n",
    )
    .unwrap();
    fs::write(
        root.join("database.env.example"),
        "DATABASE_URL=\nPROVIDER_ONLY=\n",
    )
    .unwrap();
    fs::write(
        &config,
        r#"version: 3
project: compose-e2e
logs:
  redact_patterns: [private-value]
runtimes:
  infra:
    type: compose
    project_name: compose_e2e
    files: [compose.yaml]
    reconcile: true
    generated_files: [runtime.generated.yaml]
  local:
    type: process
environment_providers:
  vault:
    type: one-password
tasks:
  prepare:
    command: [./prepare, "argument with spaces"]
    check: [test, -f, prepared]
    doctor: [./doctor-probe]
    env_from:
      - provider: vault
        reference: op://Development/database/environment
        schema: database.env.example
        cache: .hum/cache/database.env
services:
  database:
    runtime: infra
    target: database
    depends_on: [prepare]
    env_overrides:
      DATABASE_URL: postgres://runtime-overlay
    env_from:
      - provider: vault
        reference: op://Development/database/environment
        schema: database.env.example
        cache: .hum/cache/database.env
  api:
    runtime: local
    command: "true"
    depends_on: [database]
templates:
  all:
    services: [database]
  infrastructure:
    services: [database]
"#,
    )
    .unwrap();
    let prepare = root.join("prepare");
    fs::write(
        &prepare,
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$1\" >> task.log\nprintf 'services: {}\\n' > runtime.generated.yaml\n: > prepared\n",
    )
    .unwrap();
    fs::set_permissions(&prepare, fs::Permissions::from_mode(0o700)).unwrap();
    let doctor_probe = root.join("doctor-probe");
    fs::write(
        &doctor_probe,
        "#!/bin/sh\nset -eu\ntest -z \"${DATABASE_URL:-}\"\ntest -f compose.yaml\n",
    )
    .unwrap();
    fs::set_permissions(&doctor_probe, fs::Permissions::from_mode(0o700)).unwrap();

    let fake_docker = root.join("bin/docker");
    fs::write(
        &fake_docker,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$HUM_FAKE_DOCKER_LOG"
env | grep '^HUM_RUNTIME_' >> "$HUM_FAKE_DOCKER_ENV" || true
case "$*" in
  *"config --format json --no-interpolate"*)
    printf '%s\n' '{"services":{"database":{"image":"postgres:17","labels":{"source":"${DATABASE_URL}"},"environment":{"DATABASE_URL":"private-value","PUBLIC":"visible"}}}}'
    ;;
  *"ps --status running --services"*)
    if [ -f "$HUM_FAKE_DOCKER_STATE" ]; then printf '%s\n' database; fi
    ;;
  *"ps --all --services"*)
    if [ -f "$HUM_FAKE_DOCKER_STATE" ]; then printf '%s\n' database; fi
    ;;
  *"up --detach --wait database"*)
    : > "$HUM_FAKE_DOCKER_STATE"
    ;;
  *"stop database"*)
    rm -f "$HUM_FAKE_DOCKER_STATE"
    ;;
  *"logs --tail 5 database"*)
    printf '%s\n' 'compose-log-line private-value'
    ;;
  *"down --volumes --remove-orphans"*)
    rm -f "$HUM_FAKE_DOCKER_STATE"
    ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&fake_docker, fs::Permissions::from_mode(0o700)).unwrap();
    let fake_op = root.join("bin/op");
    fs::write(
        &fake_op,
        "#!/bin/sh\nset -eu\nprintf '%s\\n' call >> \"$HUM_FAKE_OP_LOG\"\nprintf '%s\\n' 'DATABASE_URL=postgres://private-value' 'PROVIDER_ONLY=available'\n",
    )
    .unwrap();
    fs::set_permissions(&fake_op, fs::Permissions::from_mode(0o700)).unwrap();

    let plan = hum(
        &root,
        &config,
        &["plan", "api", "--json", "--exclude", "infrastructure"],
    );
    assert_success(&plan);
    let plan_json: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(plan_json["roots"], serde_json::json!(["api"]));
    assert_eq!(
        plan_json["excluded_templates"],
        serde_json::json!(["infrastructure"])
    );
    let database = plan_json["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| unit["name"] == "database")
        .unwrap();
    assert!(database["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "dependency of api"));
    let warning = String::from_utf8_lossy(&plan.stderr);
    assert!(warning.contains("reintroduced from excluded template 'infrastructure'"));
    assert!(!root.join(".hum/cache/database.env").exists());

    let rendered = hum(&root, &config, &["config", "compose", "--format", "json"]);
    assert_success(&rendered);
    let rendered = String::from_utf8_lossy(&rendered.stdout);
    assert!(rendered.contains("postgres:17"));
    assert!(rendered.contains("<redacted>"));
    assert!(rendered.contains("${DATABASE_URL}"));
    assert!(!rendered.contains("private-value"));
    assert!(!rendered.contains("visible"));
    assert!(fs::read_to_string(root.join("docker.log"))
        .unwrap()
        .contains("config --format json --no-interpolate"));

    let blocked = hum(
        &root,
        &config,
        &["plan", "api", "--exclude-service", "database"],
    );
    assert_eq!(blocked.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&blocked.stderr)
        .contains("cannot exclude service 'database' because 'api' depends on it"));

    let doctor = hum(&root, &config, &["doctor", "--exclude", "infrastructure"]);
    assert_success(&doctor);
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("All checks passed"));

    let selected_doctor = hum(&root, &config, &["doctor"]);
    assert_success(&selected_doctor);
    let selected_doctor_output = String::from_utf8_lossy(&selected_doctor.stdout);
    assert!(selected_doctor_output
        .lines()
        .any(|line| { line.split_whitespace().next() == Some("prepare") && line.contains("4/4") }));
    fs::remove_file(root.join("docker.log")).unwrap();

    let excluded_sync = hum(
        &root,
        &config,
        &["secrets", "sync", "--exclude", "infrastructure"],
    );
    assert_success(&excluded_sync);
    assert!(
        String::from_utf8_lossy(&excluded_sync.stdout).contains("refreshed 0 environment source")
    );
    assert!(!root.join(".hum/cache/database.env").exists());

    let excluded_start = hum(&root, &config, &["start", "--exclude", "infrastructure"]);
    assert_success(&excluded_start);
    assert!(!root.join("docker.log").exists());

    let sync = hum(&root, &config, &["secrets", "sync"]);
    assert_success(&sync);
    let sync_output = String::from_utf8_lossy(&sync.stdout);
    assert!(sync_output.contains("refreshed 1 environment source"));
    assert!(!sync_output.contains("private-value"));
    let cache = root.join(".hum/cache/database.env");
    assert_eq!(
        fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let start = hum(&root, &config, &["start"]);
    assert_success(&start);
    assert!(String::from_utf8_lossy(&start.stdout).contains("✓ started: database"));
    assert_eq!(
        fs::read_to_string(root.join("op.log"))
            .unwrap()
            .lines()
            .count(),
        2,
        "one read occurs in the secrets-sync process; within the later start process the task and service share one memoized read",
    );

    let repeated = hum(&root, &config, &["start"]);
    assert_success(&repeated);
    assert!(String::from_utf8_lossy(&repeated.stdout).contains("✓ reconciled: database"));
    let up_count_before_reconcile = fs::read_to_string(root.join("docker.log"))
        .unwrap()
        .lines()
        .filter(|line| line.contains("up --detach --wait database"))
        .count();
    let reconciled = hum_with_env(
        &root,
        &config,
        &["start"],
        &[("DATABASE_URL", "postgres://updated-private-value")],
    );
    assert_success(&reconciled);
    assert!(String::from_utf8_lossy(&reconciled.stderr)
        .contains("env_overrides replace provider-backed keys: DATABASE_URL"));
    assert!(String::from_utf8_lossy(&reconciled.stdout).contains("✓ reconciled: database"));
    let up_count_after_reconcile = fs::read_to_string(root.join("docker.log"))
        .unwrap()
        .lines()
        .filter(|line| line.contains("up --detach --wait database"))
        .count();
    assert_eq!(up_count_after_reconcile, up_count_before_reconcile + 1);
    assert_eq!(
        fs::read_to_string(root.join("task.log")).unwrap(),
        "argument with spaces\n"
    );

    let status = hum(&root, &config, &["status"]);
    assert_success(&status);
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.contains("database"));
    assert!(status.contains("running"));
    assert!(status.contains("compose_e2e"));

    let logs = hum(&root, &config, &["logs", "database", "--lines", "5"]);
    assert_success(&logs);
    let logs = String::from_utf8_lossy(&logs.stdout);
    assert!(logs.contains("compose-log-line [REDACTED]"));
    assert!(!logs.contains("private-value"));

    let generated = root.join("state/hum/compose-e2e/compose/infra.generated.yaml");
    let generated_contents = fs::read_to_string(generated).unwrap();
    assert!(generated_contents.contains("HUM_RUNTIME_"));
    assert!(!generated_contents.contains("private-value"));
    let runtime_environment = fs::read_to_string(root.join("docker.env")).unwrap();
    assert!(runtime_environment.contains("postgres://runtime-overlay"));
    assert!(runtime_environment.contains("postgres://updated-private-value"));
    assert!(!runtime_environment.contains("postgres://private-value"));

    let stop = hum(&root, &config, &["stop"]);
    assert_success(&stop);
    assert!(String::from_utf8_lossy(&stop.stdout).contains("✓ stopped: database"));
    assert!(!root.join("docker.state").exists());

    assert_success(&hum(&root, &config, &["start"]));
    let reset = hum(&root, &config, &["reset", "--yes"]);
    assert_success(&reset);
    assert!(String::from_utf8_lossy(&reset.stdout).contains("✓ reset Compose data"));
    assert!(!root.join("docker.state").exists());

    let docker_log = fs::read_to_string(root.join("docker.log")).unwrap();
    assert!(docker_log.contains("--project-name compose_e2e"));
    assert!(docker_log.contains("--file"));
    assert!(docker_log.contains("runtime.generated.yaml"));
    assert!(docker_log.contains("up --detach --wait database"));
    assert!(docker_log.contains("stop database"));
    assert!(docker_log.contains("logs --tail 5 database"));
    assert!(docker_log.contains("--profile * down --volumes --remove-orphans"));

    let up_count_before_provider_failure = docker_log
        .lines()
        .filter(|line| line.contains("up --detach --wait database"))
        .count();
    fs::remove_file(root.join(".hum/cache/database.env")).unwrap();
    fs::write(&fake_op, "#!/bin/sh\nexit 23\n").unwrap();
    let provider_failure = hum(&root, &config, &["start"]);
    assert!(!provider_failure.status.success());
    assert!(String::from_utf8_lossy(&provider_failure.stderr)
        .contains("required environment source from provider 'vault' is unavailable"));
    let up_count_after_provider_failure = fs::read_to_string(root.join("docker.log"))
        .unwrap()
        .lines()
        .filter(|line| line.contains("up --detach --wait database"))
        .count();
    assert_eq!(
        up_count_after_provider_failure, up_count_before_provider_failure,
        "Compose must not start with an unavailable required provider source",
    );
}

fn hum(root: &Path, config: &Path, arguments: &[&str]) -> Output {
    hum_with_env(root, config, arguments, &[])
}

fn hum_with_env(
    root: &Path,
    config: &Path,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Output {
    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        existing_path.to_string_lossy()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_hum"));
    command
        .arg("--config")
        .arg(config)
        .arg("compose-e2e")
        .arg("all")
        .args(arguments)
        .env("PATH", path)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("HUM_FAKE_DOCKER_STATE", root.join("docker.state"))
        .env("HUM_FAKE_DOCKER_LOG", root.join("docker.log"))
        .env("HUM_FAKE_DOCKER_ENV", root.join("docker.env"));
    command.env("HUM_FAKE_OP_LOG", root.join("op.log"));
    command.envs(environment.iter().copied());
    command.output().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
