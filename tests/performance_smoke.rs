#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const DEFAULT_START_BUDGET_MS: u64 = 2_000;
const DEFAULT_FIRST_FRAME_BUDGET_MS: u64 = 250;
const DEFAULT_TUI_CPU_BUDGET_PERCENT: f64 = 2.0;
const DEFAULT_TUI_RSS_BUDGET_MIB: f64 = 60.0;
const DEFAULT_SINK_CPU_BUDGET_PERCENT: f64 = 1.0;
const DEFAULT_SINK_RSS_BUDGET_MIB: f64 = 80.0;
const SAMPLE_COUNT: usize = 12;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

struct Fixture {
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
    process_groups: Vec<i32>,
    log_sinks: Vec<(u32, u64)>,
}

impl Fixture {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hum-performance-smoke-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let config = root.join("hum.yaml");
        let state = root.join("state");
        let mut contents = "version: 2\nproject: polling-benchmark\nservices:\n".to_string();
        for index in 1..=10 {
            contents.push_str(&format!(
                "  service-{index:02}: {{ command: \"sleep 300\" }}\n"
            ));
        }
        contents.push_str("templates:\n  all:\n    services:\n");
        for index in 1..=10 {
            contents.push_str(&format!("      - service-{index:02}\n"));
        }
        fs::write(&config, contents).unwrap();
        Self {
            root,
            config,
            state,
            process_groups: Vec::new(),
            log_sinks: Vec::new(),
        }
    }

    fn command(&self, action: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hum"));
        command
            .env("XDG_STATE_HOME", &self.state)
            .env("TERM", "xterm-256color")
            .arg("--config")
            .arg(&self.config)
            .args(["polling-benchmark", "all", action]);
        command
    }

    fn remember_process_groups(&mut self) {
        let runtime = self.state.join("hum/polling-benchmark/runtime");
        let Ok(entries) = fs::read_dir(runtime) else {
            return;
        };
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = fs::read(path) {
                if let Ok(entry) = serde_json::from_slice::<Value>(&bytes) {
                    if let Some(pgid) = entry["pgid"].as_i64() {
                        self.process_groups.push(pgid as i32);
                    }
                    if let Some((pid, start_time)) = entry["log_sink_pid"]
                        .as_u64()
                        .zip(entry["log_sink_start_time"].as_u64())
                    {
                        self.log_sinks.push((pid as u32, start_time));
                    }
                }
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let stopped = !self.process_groups.is_empty()
            && self
                .command("stop")
                .output()
                .is_ok_and(|output| output.status.success());
        if stopped
            && wait_for_process_groups_gone(&self.process_groups, Duration::from_secs(2))
            && wait_for_processes_gone(&self.log_sinks, Duration::from_secs(2))
        {
            self.process_groups.clear();
            self.log_sinks.clear();
        }
        if self.process_groups.is_empty() && self.log_sinks.is_empty() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

struct TuiProcess {
    child: Child,
    master: File,
}

impl Drop for TuiProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "release-only performance gate; run with cargo test --release --test performance_smoke -- --ignored"]
fn ten_service_startup_tui_cpu_and_rss_stay_within_budget() {
    let mut fixture = Fixture::new();
    let start_budget = duration_budget("HUM_START_BUDGET_MS", DEFAULT_START_BUDGET_MS);
    let first_frame_budget =
        duration_budget("HUM_FIRST_FRAME_BUDGET_MS", DEFAULT_FIRST_FRAME_BUDGET_MS);
    let tui_cpu_budget =
        numeric_budget("HUM_TUI_CPU_BUDGET_PERCENT", DEFAULT_TUI_CPU_BUDGET_PERCENT);
    let tui_rss_budget = numeric_budget("HUM_TUI_RSS_BUDGET_MIB", DEFAULT_TUI_RSS_BUDGET_MIB);
    let sink_cpu_budget = numeric_budget(
        "HUM_SINK_CPU_BUDGET_PERCENT",
        DEFAULT_SINK_CPU_BUDGET_PERCENT,
    );
    let sink_rss_budget = numeric_budget("HUM_SINK_RSS_BUDGET_MIB", DEFAULT_SINK_RSS_BUDGET_MIB);

    let started_at = Instant::now();
    let started = fixture.command("start").output().unwrap();
    let start_elapsed = started_at.elapsed();
    fixture.remember_process_groups();
    assert_success(&started);
    assert!(
        start_elapsed <= start_budget,
        "ten-service start took {start_elapsed:?}, budget is {start_budget:?}"
    );

    let frame_started = Instant::now();
    let mut tui = spawn_tui(&fixture);
    wait_for_output(
        &mut tui.master,
        b"services",
        first_frame_budget,
        "rendered service table",
    );
    let first_frame_elapsed = frame_started.elapsed();

    assert_eq!(
        fixture.log_sinks.len(),
        10,
        "expected one log sink per service"
    );
    let sink_pids = fixture
        .log_sinks
        .iter()
        .map(|(pid, _)| *pid)
        .collect::<Vec<_>>();
    let metrics = sample_idle_resources(&mut tui.master, tui.child.id(), &sink_pids);

    drain_available(&mut tui.master);
    tui.master.write_all(b"q").unwrap();
    wait_for_output(
        &mut tui.master,
        b"leave",
        Duration::from_secs(2),
        "quit confirmation dialog",
    );
    tui.master.write_all(b"l").unwrap();
    wait_for_exit(&mut tui.child, Duration::from_secs(2));

    let stopped = fixture.command("stop").output().unwrap();
    assert_success(&stopped);
    assert_no_runtime_entries(&fixture.state);
    assert_process_groups_gone(&fixture.process_groups);
    assert_processes_gone(&fixture.log_sinks);
    fixture.process_groups.clear();
    fixture.log_sinks.clear();

    eprintln!(
        "ten-service smoke: start={start_elapsed:?} first_frame={first_frame_elapsed:?} tui_cpu={:.2}% tui_rss={:.2}MiB sink_cpu={:.2}% sink_rss={:.2}MiB",
        metrics.tui_average_cpu,
        bytes_to_mib(metrics.tui_peak_rss),
        metrics.sink_average_cpu,
        bytes_to_mib(metrics.sink_peak_rss),
    );
    assert!(
        first_frame_elapsed <= first_frame_budget,
        "first TUI frame took {first_frame_elapsed:?}, budget is {first_frame_budget:?}"
    );
    assert!(
        metrics.tui_average_cpu <= tui_cpu_budget,
        "average TUI CPU was {:.2}%, budget is {tui_cpu_budget:.2}%",
        metrics.tui_average_cpu,
    );
    assert!(
        bytes_to_mib(metrics.tui_peak_rss) <= tui_rss_budget,
        "peak TUI RSS was {:.2}MiB, budget is {:.2}MiB",
        bytes_to_mib(metrics.tui_peak_rss),
        tui_rss_budget,
    );
    assert!(
        metrics.sink_average_cpu <= sink_cpu_budget,
        "aggregate log-sink CPU was {:.2}%, budget is {sink_cpu_budget:.2}%",
        metrics.sink_average_cpu,
    );
    assert!(
        bytes_to_mib(metrics.sink_peak_rss) <= sink_rss_budget,
        "aggregate log-sink RSS was {:.2}MiB, budget is {sink_rss_budget:.2}MiB",
        bytes_to_mib(metrics.sink_peak_rss),
    );
}

fn spawn_tui(fixture: &Fixture) -> TuiProcess {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    #[cfg(target_os = "macos")]
    let mut window = libc::winsize {
        ws_row: 30,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    #[cfg(target_os = "linux")]
    let window = libc::winsize {
        ws_row: 30,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    #[cfg(target_os = "macos")]
    let opened = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut window,
        )
    };
    #[cfg(target_os = "linux")]
    let opened = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &window,
        )
    };
    assert_eq!(
        opened,
        0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );

    let master = unsafe { File::from_raw_fd(master_fd) };
    let stdin = unsafe { File::from_raw_fd(slave_fd) };
    let stdout = stdin.try_clone().unwrap();
    let stderr = stdin.try_clone().unwrap();
    let child = fixture
        .command("tui")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();

    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    TuiProcess { child, master }
}

fn wait_for_output(master: &mut File, marker: &[u8], budget: Duration, description: &str) {
    let deadline = Instant::now() + budget;
    let mut buffer = [0_u8; 4096];
    let mut observed = Vec::new();
    loop {
        match master.read(&mut buffer) {
            Ok(read) if read > 0 => {
                observed.extend_from_slice(&buffer[..read]);
                if observed
                    .windows(marker.len())
                    .any(|window| window == marker)
                {
                    return;
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("failed to read TUI output: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "TUI emitted no {description} within {budget:?}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn drain_available(master: &mut File) {
    let mut buffer = [0_u8; 8192];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => return,
            Err(error) => panic!("failed to drain TUI output: {error}"),
        }
    }
}

struct ResourceMetrics {
    tui_average_cpu: f64,
    tui_peak_rss: u64,
    sink_average_cpu: f64,
    sink_peak_rss: u64,
}

fn sample_idle_resources(master: &mut File, tui_pid: u32, sink_pids: &[u32]) -> ResourceMetrics {
    let tui_pid = Pid::from_u32(tui_pid);
    let sink_pids = sink_pids
        .iter()
        .copied()
        .map(Pid::from_u32)
        .collect::<Vec<_>>();
    let mut all_pids = sink_pids.clone();
    all_pids.push(tui_pid);
    let refresh = ProcessRefreshKind::new().with_cpu().with_memory();
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&all_pids), true, refresh);
    thread::sleep(SAMPLE_INTERVAL);

    let mut tui_cpu = 0.0_f64;
    let mut tui_peak_rss = 0_u64;
    let mut sink_cpu = 0.0_f64;
    let mut sink_peak_rss = 0_u64;
    for _ in 0..SAMPLE_COUNT {
        drain_available(master);
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&all_pids), true, refresh);
        let tui = system
            .process(tui_pid)
            .expect("TUI process disappeared during sampling");
        tui_cpu += f64::from(tui.cpu_usage());
        tui_peak_rss = tui_peak_rss.max(tui.memory());
        let mut interval_sink_cpu = 0.0_f64;
        let mut interval_sink_rss = 0_u64;
        for pid in &sink_pids {
            let sink = system
                .process(*pid)
                .expect("log sink disappeared during sampling");
            interval_sink_cpu += f64::from(sink.cpu_usage());
            interval_sink_rss += sink.memory();
        }
        sink_cpu += interval_sink_cpu;
        sink_peak_rss = sink_peak_rss.max(interval_sink_rss);
        thread::sleep(SAMPLE_INTERVAL);
    }
    ResourceMetrics {
        tui_average_cpu: tui_cpu / SAMPLE_COUNT as f64,
        tui_peak_rss,
        sink_average_cpu: sink_cpu / SAMPLE_COUNT as f64,
        sink_peak_rss,
    }
}

fn wait_for_exit(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "TUI exited with {status}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "TUI did not exit after explicit leave"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_no_runtime_entries(state: &Path) {
    let runtime = state.join("hum/polling-benchmark/runtime");
    let entries = fs::read_dir(runtime)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    assert!(entries.is_empty(), "runtime entries remain: {entries:?}");
}

fn assert_process_groups_gone(process_groups: &[i32]) {
    assert!(
        wait_for_process_groups_gone(process_groups, Duration::from_secs(2)),
        "one or more fixture process groups survived stop: {process_groups:?}"
    );
}

fn wait_for_process_groups_gone(process_groups: &[i32], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let all_gone = process_groups.iter().all(|pgid| {
            let result = unsafe { libc::kill(-pgid, 0) };
            result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        });
        if all_gone {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_processes_gone(processes: &[(u32, u64)]) {
    assert!(
        wait_for_processes_gone(processes, Duration::from_secs(2)),
        "one or more fixture log sinks survived stop: {processes:?}"
    );
}

fn wait_for_processes_gone(processes: &[(u32, u64)], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let pids = processes
        .iter()
        .map(|(pid, _)| Pid::from_u32(*pid))
        .collect::<Vec<_>>();
    loop {
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::new(),
        );
        let all_gone = processes.iter().all(|(pid, start_time)| {
            system
                .process(Pid::from_u32(*pid))
                .is_none_or(|process| process.start_time() != *start_time)
        });
        if all_gone {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn duration_budget(name: &str, default_ms: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default_ms),
    )
}

fn numeric_budget(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn bytes_to_mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}
