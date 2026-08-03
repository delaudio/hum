#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sysinfo::System;

struct Cleanup {
    root: PathBuf,
    process_groups: Vec<i32>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for pgid in &self.process_groups {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn detached_start_survives_cli_exit_and_reconciles_concurrent_and_stale_state() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("hum-cli-detached-{}-{unique}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let mut cleanup = Cleanup {
        root: root.clone(),
        process_groups: Vec::new(),
    };
    let config = root.join("hum.yaml");
    let state = root.join("state");
    fs::write(
        &config,
        r#"version: 2
project: e2e
services:
  worker:
    command: "echo cli-detached-ok; while :; do echo tick; sleep 0.1; done"
templates:
  all:
    services: [worker]
"#,
    )
    .unwrap();

    let first = start_command(&config, &state).spawn().unwrap();
    let second = start_command(&config, &state).spawn().unwrap();
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert_success(&first);
    assert_success(&second);
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&second.stdout)
    );
    assert_eq!(output.matches("✓ started: worker").count(), 1);
    assert_eq!(output.matches("✓ already running: worker").count(), 1);

    let entry_path = state.join("hum/e2e/runtime/worker.json");
    let first_entry = read_entry(&entry_path);
    let first_pid = first_entry["pid"].as_u64().unwrap() as u32;
    let first_pgid = first_entry["pgid"].as_i64().unwrap() as i32;
    cleanup.process_groups.push(first_pgid);

    let system = System::new_all();
    let service = system
        .process(sysinfo::Pid::from_u32(first_pid))
        .expect("detached service should outlive both hum invocations");
    assert_eq!(service.parent().map(sysinfo::Pid::as_u32), Some(1));

    let stdout_log = PathBuf::from(first_entry["stdout_log"].as_str().unwrap());
    wait_for_log(&stdout_log, "cli-detached-ok");
    let initial_len = fs::metadata(&stdout_log).unwrap().len();
    thread::sleep(Duration::from_millis(250));
    assert!(fs::metadata(&stdout_log).unwrap().len() > initial_len);

    unsafe {
        assert_eq!(libc::kill(-first_pgid, libc::SIGKILL), 0);
    }
    wait_for_group_exit(first_pgid);

    let restarted = start_command(&config, &state).output().unwrap();
    assert_success(&restarted);
    assert!(String::from_utf8_lossy(&restarted.stdout).contains("✓ started: worker"));
    let second_entry = read_entry(&entry_path);
    let second_pgid = second_entry["pgid"].as_i64().unwrap() as i32;
    assert_ne!(second_entry["runtime_token"], first_entry["runtime_token"]);
    cleanup.process_groups.push(second_pgid);
}

fn start_command(config: &Path, state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hum"));
    command
        .env("XDG_STATE_HOME", state)
        .arg("--config")
        .arg(config)
        .args(["e2e", "all", "start"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn read_entry(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "hum failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_log(path: &Path, expected: &str) {
    for _ in 0..40 {
        if fs::read_to_string(path).is_ok_and(|contents| contents.contains(expected)) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not contain {expected:?}", path.display());
}

fn wait_for_group_exit(pgid: i32) {
    for _ in 0..40 {
        let result = unsafe { libc::kill(-pgid, 0) };
        if result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("process group {pgid} did not exit");
}
