#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sysinfo::System;

static PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

fn process_test_guard() -> MutexGuard<'static, ()> {
    PROCESS_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
    let _process_guard = process_test_guard();
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
    let first_sink_pid = first_entry["log_sink_pid"].as_u64().unwrap() as u32;
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
    let template_logs = hum_command(&config, &state, &["logs", "-n", "20"])
        .output()
        .unwrap();
    assert_success(&template_logs);
    assert!(String::from_utf8_lossy(&template_logs.stdout).contains("cli-detached-ok"));

    let restarted = hum_command(&config, &state, &["restart", "--timeout", "1s"])
        .output()
        .unwrap();
    assert_success(&restarted);
    assert!(String::from_utf8_lossy(&restarted.stdout).contains("✓ restarted: worker"));
    let restarted_entry = read_entry(&entry_path);
    let restarted_pgid = restarted_entry["pgid"].as_i64().unwrap() as i32;
    let restarted_sink_pid = restarted_entry["log_sink_pid"].as_u64().unwrap() as u32;
    assert_ne!(
        restarted_entry["runtime_token"],
        first_entry["runtime_token"]
    );
    assert_ne!(restarted_sink_pid, first_sink_pid);
    assert!(!process_exists(first_sink_pid));
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
    assert!(!process_exists(restarted_sink_pid));

    let stale_replaced = hum_command(&config, &state, &["start"]).output().unwrap();
    assert_success(&stale_replaced);
    assert!(String::from_utf8_lossy(&stale_replaced.stdout).contains("✓ started: worker"));
    let replacement_entry = read_entry(&entry_path);
    let replacement_pgid = replacement_entry["pgid"].as_i64().unwrap() as i32;
    let replacement_sink_pid = replacement_entry["log_sink_pid"].as_u64().unwrap() as u32;
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
    assert!(!process_exists(replacement_sink_pid));

    let already_stopped = hum_command(&config, &state, &["stop"]).output().unwrap();
    assert_success(&already_stopped);
    assert!(String::from_utf8_lossy(&already_stopped.stdout).contains("✓ already stopped: worker"));

    let final_status = hum_command(&config, &state, &["status"]).output().unwrap();
    assert_success(&final_status);
    assert!(String::from_utf8_lossy(&final_status.stdout).contains("exited"));
    assert_eq!(
        fs::read_to_string(state.join("hum/e2e/runtime/worker.exit"))
            .unwrap()
            .trim(),
        "143"
    );

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

#[test]
fn noisy_service_logs_rotate_redact_and_survive_crash() {
    let _process_guard = process_test_guard();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "hum-cli-log-rotation-{}-{unique}",
        std::process::id()
    ));
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
logs:
  max_file_bytes: 128
  rotated_files: 2
  max_line_bytes: 32
  redact_patterns: ["token=[^ ]+"]
services:
  worker:
    command: "i=0; while [ $i -lt 1000 ]; do echo token=secret line-$i-abcdefghijklmnopqrstuvwxyz; i=$((i+1)); done; echo token=secret complete; echo token=secret error >&2; sleep 300"
templates:
  all:
    services: [worker]
"#,
    )
    .unwrap();

    let started = hum_command(&config, &state, &["start"]).output().unwrap();
    assert_success(&started);
    let entry_path = state.join("hum/e2e/runtime/worker.json");
    let entry = read_entry(&entry_path);
    let pgid = entry["pgid"].as_i64().unwrap() as i32;
    cleanup.process_groups.push(pgid);
    let stdout = PathBuf::from(entry["stdout_log"].as_str().unwrap());
    wait_for_log_set(&stdout, "complete", 2);

    let status = hum_command(&config, &state, &["status"]).output().unwrap();
    assert_success(&status);
    assert!(String::from_utf8_lossy(&status.stdout).contains("running"));

    let logs = hum_command(&config, &state, &["logs", "worker", "-n", "20"])
        .output()
        .unwrap();
    assert_success(&logs);
    let visible = String::from_utf8_lossy(&logs.stdout);
    assert!(visible.contains("[REDACTED]"));
    assert!(visible.contains("[stderr]"));
    assert!(!visible.contains("token=secret"));

    let files = [
        stdout.clone(),
        PathBuf::from(format!("{}.1", stdout.display())),
        PathBuf::from(format!("{}.2", stdout.display())),
    ];
    assert!(files
        .iter()
        .all(|path| fs::metadata(path).unwrap().len() <= 128));
    assert!(
        files
            .iter()
            .map(|path| fs::metadata(path).unwrap().len())
            .sum::<u64>()
            <= 128 * 3
    );
    assert!(!PathBuf::from(format!("{}.3", stdout.display())).exists());

    unsafe {
        assert_eq!(libc::kill(-pgid, libc::SIGKILL), 0);
    }
    wait_for_group_exit(pgid);
    thread::sleep(Duration::from_millis(100));
    let after_crash = hum_command(&config, &state, &["logs", "worker", "-n", "20"])
        .output()
        .unwrap();
    assert_success(&after_crash);
    assert!(String::from_utf8_lossy(&after_crash.stdout).contains("[REDACTED]"));
}

#[test]
fn detached_sink_reads_export_configuration_from_inherited_fd4() {
    let _process_guard = process_test_guard();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "hum-cli-fd4-export-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let mut cleanup = Cleanup {
        root: root.clone(),
        process_groups: Vec::new(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/events", listener.local_addr().unwrap());
    let (body_sender, body_receiver) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let started = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break Some(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() >= Duration::from_secs(3) {
                        break None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break None,
            }
        };
        let Some(mut stream) = stream.take() else {
            body_sender.send(None).unwrap();
            return;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut content_length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() || header.is_empty() {
                body_sender.send(None).unwrap();
                return;
            }
            if header == "\r\n" {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; content_length];
        if reader.read_exact(&mut body).is_err() {
            body_sender.send(None).unwrap();
            return;
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        body_sender.send(Some(body)).unwrap();
    });
    let config = root.join("hum.yaml");
    let state = root.join("state");
    fs::write(
        &config,
        format!(
            r#"version: 2
project: e2e
logs:
  redact_patterns: ["token=[^ ]+"]
  exporters:
    - type: http
      endpoint: {endpoint}
      timeout: 750ms
services:
  worker:
    command: "echo token=secret fd4-export-ok; sleep 300"
templates:
  all:
    services: [worker]
"#
        ),
    )
    .unwrap();

    let started = hum_command(&config, &state, &["start"]).output().unwrap();
    assert_success(&started);
    let entry_path = state.join("hum/e2e/runtime/worker.json");
    let entry = read_entry(&entry_path);
    cleanup
        .process_groups
        .push(entry["pgid"].as_i64().unwrap() as i32);
    let body = body_receiver
        .recv_timeout(Duration::from_secs(4))
        .unwrap()
        .expect("real sink did not send an exported event");
    let event = body
        .split(|byte| *byte == b'\n')
        .find(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .unwrap();
    assert_eq!(event["message"], "[REDACTED] fd4-export-ok");
    assert_eq!(event["service"]["name"], "worker");
    assert_eq!(event["hum"]["project"], "e2e");
    assert!(!String::from_utf8_lossy(&body).contains("token=secret"));

    let stopped = hum_command(&config, &state, &["stop", "--timeout", "1s"])
        .output()
        .unwrap();
    assert_success(&stopped);
    server.join().unwrap();
}

#[test]
fn detached_command_records_a_natural_exit_code() {
    let _process_guard = process_test_guard();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("hum-cli-exit-code-{}-{unique}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let _cleanup = Cleanup {
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
    command: "sleep 0.2; exit 7"
templates:
  all:
    services: [worker]
"#,
    )
    .unwrap();

    let started = hum_command(&config, &state, &["start"]).output().unwrap();
    assert_success(&started);
    let mut status_text = String::new();
    for _ in 0..40 {
        let status = hum_command(&config, &state, &["status"]).output().unwrap();
        assert_success(&status);
        status_text = String::from_utf8_lossy(&status.stdout).to_string();
        if status_text.contains("exited") {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    assert!(status_text.contains("exited"), "status was: {status_text}");
    assert_eq!(
        fs::read_to_string(state.join("hum/e2e/runtime/worker.exit"))
            .unwrap()
            .trim(),
        "7"
    );
}

#[test]
fn v3_status_ignores_tasks_in_the_service_dependency_order() {
    let _process_guard = process_test_guard();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "hum-cli-v3-task-status-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    let _cleanup = Cleanup {
        root: root.clone(),
        process_groups: Vec::new(),
    };
    let config = root.join("hum.yaml");
    let state = root.join("state");
    fs::write(
        &config,
        r#"version: 3
project: e2e
runtimes:
  local:
    type: process
tasks:
  setup:
    command: ["true"]
services:
  worker:
    runtime: local
    command: "sleep 300"
    depends_on: [setup]
templates:
  all:
    services: [worker]
"#,
    )
    .unwrap();

    let status = hum_command(&config, &state, &["status"]).output().unwrap();
    assert_success(&status);
    let output = String::from_utf8_lossy(&status.stdout);
    assert!(output.contains("worker"), "status was: {output}");
    assert!(!output.contains("setup"), "status was: {output}");
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

fn wait_for_log_set(path: &Path, expected: &str, rotated_files: usize) {
    for _ in 0..100 {
        let found = (0..=rotated_files).any(|index| {
            let candidate = if index == 0 {
                path.to_path_buf()
            } else {
                PathBuf::from(format!("{}.{index}", path.display()))
            };
            fs::read_to_string(candidate).is_ok_and(|contents| contents.contains(expected))
        });
        if found {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "log set for {} did not contain {expected:?}",
        path.display()
    );
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

fn process_exists(pid: u32) -> bool {
    let system = System::new_all();
    system.process(sysinfo::Pid::from_u32(pid)).is_some()
}
