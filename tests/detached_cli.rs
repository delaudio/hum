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

    let first = hum_command(&config, &state, &["start"]).spawn().unwrap();
    let second = hum_command(&config, &state, &["start"]).spawn().unwrap();
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

    let status = hum_command(&config, &state, &["status"]).output().unwrap();
    assert_success(&status);
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(status_text.contains("worker"));
    assert!(status_text.contains("running"));
    assert!(status_text.contains(&first_pid.to_string()));

    let logs = hum_command(&config, &state, &["logs", "worker", "-n", "20"])
        .output()
        .unwrap();
    assert_success(&logs);
    assert!(String::from_utf8_lossy(&logs.stdout).contains("cli-detached-ok"));

    let restarted = hum_command(&config, &state, &["restart", "--timeout", "1s"])
        .output()
        .unwrap();
    assert_success(&restarted);
    assert!(String::from_utf8_lossy(&restarted.stdout).contains("✓ restarted: worker"));
    let restarted_entry = read_entry(&entry_path);
    let restarted_pgid = restarted_entry["pgid"].as_i64().unwrap() as i32;
    assert_ne!(
        restarted_entry["runtime_token"],
        first_entry["runtime_token"]
    );
    cleanup.process_groups.push(restarted_pgid);

    unsafe {
        assert_eq!(libc::kill(-restarted_pgid, libc::SIGKILL), 0);
    }
    wait_for_group_exit(restarted_pgid);

    let stale_status = hum_command(&config, &state, &["status"]).output().unwrap();
    assert_success(&stale_status);
    let stale_status_text = String::from_utf8_lossy(&stale_status.stdout);
    assert!(stale_status_text.contains("missing"));
    assert!(stale_status_text.contains("stale runtime entry removed"));
    assert!(!entry_path.exists());

    let stale_replaced = hum_command(&config, &state, &["start"]).output().unwrap();
    assert_success(&stale_replaced);
    assert!(String::from_utf8_lossy(&stale_replaced.stdout).contains("✓ started: worker"));
    let replacement_entry = read_entry(&entry_path);
    let replacement_pgid = replacement_entry["pgid"].as_i64().unwrap() as i32;
    assert_ne!(
        replacement_entry["runtime_token"],
        restarted_entry["runtime_token"]
    );
    cleanup.process_groups.push(replacement_pgid);

    let stopped = hum_command(&config, &state, &["stop", "--timeout", "1s"])
        .output()
        .unwrap();
    assert_success(&stopped);
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("✓ stopped: worker"));
    assert!(!entry_path.exists());

    let already_stopped = hum_command(&config, &state, &["stop"]).output().unwrap();
    assert_success(&already_stopped);
    assert!(String::from_utf8_lossy(&already_stopped.stdout).contains("✓ already stopped: worker"));

    let final_status = hum_command(&config, &state, &["status"]).output().unwrap();
    assert_success(&final_status);
    assert!(String::from_utf8_lossy(&final_status.stdout).contains("missing"));

    let started_for_failure = hum_command(&config, &state, &["start"]).output().unwrap();
    assert_success(&started_for_failure);
    let failure_entry = read_entry(&entry_path);
    cleanup
        .process_groups
        .push(failure_entry["pgid"].as_i64().unwrap() as i32);
    fs::remove_file(failure_entry["identity_file"].as_str().unwrap()).unwrap();
    let failed_restart = hum_command(&config, &state, &["restart", "--timeout", "10ms"])
        .output()
        .unwrap();
    assert_eq!(failed_restart.status.code(), Some(7));
    let failure_text = String::from_utf8_lossy(&failed_restart.stderr);
    assert!(failure_text.contains("project 'e2e' template 'all'"));
    assert!(failure_text.contains("worker"));
    assert!(entry_path.exists());
}

fn hum_command(config: &Path, state: &Path, action: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hum"));
    command
        .env("XDG_STATE_HOME", state)
        .arg("--config")
        .arg(config)
        .args(["e2e", "all"])
        .args(action)
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
