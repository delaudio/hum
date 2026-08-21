#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn switch_provider_receives_direct_argv_and_controls_the_exit_status() {
    let root = std::env::temp_dir().join(format!(
        "hum-switch-cli-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let _cleanup = Cleanup(root.clone());

    let config = project.join("hum.yaml");
    fs::write(
        &config,
        r#"version: 3
project: switch-e2e
runtimes:
  local:
    type: process
switch_provider:
  command: [../switch-provider, fixed]
services:
  api:
    runtime: local
    command: "true"
  worker:
    runtime: local
    command: "true"
templates:
  all:
    services: [api, worker]
  api-only:
    services: [api]
"#,
    )
    .unwrap();
    let provider = root.join("switch-provider");
    fs::write(
        &provider,
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > \"$HUM_SWITCH_LOG\"\n[ \"${2:-}\" != fail ]\n",
    )
    .unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let log = root.join("switch.log");

    let selected = Command::new(env!("CARGO_BIN_EXE_hum"))
        .current_dir(&project)
        .args([
            "--config",
            "hum.yaml",
            "switch-e2e",
            "all",
            "switch",
            "source",
            "api",
            "worker",
            "--no-start",
        ])
        .env("HUM_SWITCH_LOG", &log)
        .output()
        .unwrap();
    assert!(selected.status.success(), "{selected:?}");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "fixed\nsource\napi\nworker\n--template\nall\n--no-start\n"
    );

    let all = hum(&config, &log)
        .args(["switch-e2e", "all", "switch", "image", "--all"])
        .output()
        .unwrap();
    assert!(all.status.success(), "{all:?}");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "fixed\nimage\n--all\n--template\nall\n"
    );

    let unknown = hum(&config, &log)
        .args(["switch-e2e", "all", "switch", "source", "missing"])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(5), "{unknown:?}");

    let outside_template = hum(&config, &log)
        .args(["switch-e2e", "api-only", "switch", "source", "worker"])
        .output()
        .unwrap();
    assert_eq!(
        outside_template.status.code(),
        Some(5),
        "{outside_template:?}"
    );
    assert!(
        String::from_utf8_lossy(&outside_template.stderr)
            .contains("service 'worker' is not part of template 'api-only'"),
        "{outside_template:?}"
    );

    let failed = hum(&config, &log)
        .args(["switch-e2e", "all", "switch", "fail"])
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(12), "{failed:?}");
}

fn hum(config: &std::path::Path, log: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hum"));
    command
        .arg("--config")
        .arg(config)
        .env("HUM_SWITCH_LOG", log);
    command
}
