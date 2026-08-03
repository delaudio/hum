use std::collections::HashMap;
use std::net::IpAddr;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

const DNS_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
enum DnsEntry {
    Resolving,
    Ready {
        resolved_at: std::time::Instant,
        addresses: Vec<SocketAddr>,
    },
}

static DNS_CACHE: OnceLock<Mutex<HashMap<(String, u16), DnsEntry>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProbe {
    Listening,
    Closed,
    Unknown,
}

/// Lightweight port polling: one bounded TCP connection, with no process
/// enumeration and no `lsof` subprocess.
pub fn probe_port(port: u16) -> PortProbe {
    probe_host_port("localhost", port, Duration::from_millis(50))
}

pub fn probe_host_port(host: &str, port: u16, timeout: Duration) -> PortProbe {
    let started = std::time::Instant::now();
    let Some(addresses) = resolve_addresses(host, port, timeout) else {
        return PortProbe::Unknown;
    };
    let deadline = started + timeout;
    let mut attempted = false;
    let mut timed_out = false;
    for address in addresses {
        attempted = true;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(_) => return PortProbe::Listening,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => timed_out = true,
            Err(_) => {}
        }
    }
    if !attempted || timed_out {
        PortProbe::Unknown
    } else {
        PortProbe::Closed
    }
}

fn resolve_addresses(host: &str, port: u16, timeout: Duration) -> Option<Vec<SocketAddr>> {
    if let Ok(address) = host.parse::<IpAddr>() {
        return Some(vec![SocketAddr::new(address, port)]);
    }
    if host.eq_ignore_ascii_case("localhost") {
        return Some(vec![
            SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), port),
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port),
        ]);
    }

    let key = (host.to_string(), port);
    let cache = DNS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let mut entries = cache.lock().unwrap();
        match entries.get(&key) {
            Some(DnsEntry::Ready {
                resolved_at,
                addresses,
            }) if resolved_at.elapsed() < DNS_TTL => return Some(addresses.clone()),
            Some(DnsEntry::Resolving) => return None,
            _ => {
                entries.insert(key.clone(), DnsEntry::Resolving);
            }
        }
    }

    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let addresses = (key.0.as_str(), key.1)
            .to_socket_addrs()
            .map(|addresses| addresses.collect::<Vec<_>>())
            .unwrap_or_default();
        DNS_CACHE
            .get()
            .expect("DNS cache initialized before resolver thread")
            .lock()
            .unwrap()
            .insert(
                key,
                DnsEntry::Ready {
                    resolved_at: std::time::Instant::now(),
                    addresses: addresses.clone(),
                },
            );
        let _ = sender.send(addresses);
    });
    receiver.recv_timeout(timeout).ok()
}

/// Information about whatever is occupying a port, when we can determine it
/// (RF-15). Best-effort: process inspection differs across platforms.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PortOccupant {
    pub pid: Option<u32>,
    pub process_name: Option<String>,
    pub command: Option<String>,
}

/// RF-15: check whether `port` is free on localhost by attempting to bind
/// it. Returns `None` if free, `Some(occupant)` if taken.
pub fn check_port(port: u16) -> Option<PortOccupant> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    if TcpListener::bind(addr).is_ok() {
        return None;
    }
    Some(identify_occupant(port))
}

pub fn identify_occupant(port: u16) -> PortOccupant {
    #[cfg(test)]
    DIAGNOSTIC_CALLS.with(|calls| calls.set(calls.get() + 1));
    // Best-effort: sysinfo does not expose per-socket ownership portably, so
    // we shell out to `lsof` on unix, which is present on macOS and most
    // Linux dev boxes, and fall back to "unknown" if it isn't.
    #[cfg(unix)]
    {
        if let Ok(output) = std::process::Command::new("lsof")
            .args(["-nP", "-i", &format!(":{port}"), "-sTCP:LISTEN", "-t"])
            .output()
        {
            if let Ok(text) = String::from_utf8(output.stdout) {
                if let Some(pid_str) = text.lines().next() {
                    if let Ok(pid) = pid_str.trim().parse::<u32>() {
                        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
                        let process_pid = sysinfo::Pid::from_u32(pid);
                        let mut system = System::new();
                        system.refresh_processes_specifics(
                            ProcessesToUpdate::Some(&[process_pid]),
                            true,
                            ProcessRefreshKind::everything(),
                        );
                        let name = system
                            .process(sysinfo::Pid::from_u32(pid))
                            .map(|p| p.name().to_string_lossy().to_string());
                        let cmd = system.process(sysinfo::Pid::from_u32(pid)).map(|p| {
                            p.cmd()
                                .iter()
                                .map(|s| s.to_string_lossy().to_string())
                                .collect::<Vec<_>>()
                                .join(" ")
                        });
                        return PortOccupant {
                            pid: Some(pid),
                            process_name: name,
                            command: cmd,
                        };
                    }
                }
            }
        }
    }

    PortOccupant {
        pid: None,
        process_name: None,
        command: None,
    }
}

#[cfg(test)]
thread_local! {
    static DIAGNOSTIC_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn reset_diagnostic_call_count() {
    DIAGNOSTIC_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub fn diagnostic_call_count() -> usize {
    DIAGNOSTIC_CALLS.with(std::cell::Cell::get)
}

pub fn belongs_to_process_tree(pid: u32, root_pid: u32) -> bool {
    use sysinfo::System;

    let system = System::new_all();
    let root = sysinfo::Pid::from_u32(root_pid);
    let mut current = Some(sysinfo::Pid::from_u32(pid));
    while let Some(pid) = current {
        if pid == root {
            return true;
        }
        current = system.process(pid).and_then(sysinfo::Process::parent);
    }
    false
}

pub fn belongs_to_process_group(pid: u32, pgid: i32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::getpgid(pid as i32) == pgid }
    }
    #[cfg(not(unix))]
    {
        pid as i32 == pgid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lightweight_probe_distinguishes_listener_from_closed_port() {
        let Ok(listener) = TcpListener::bind(("127.0.0.1", 0)) else {
            // Some CI sandboxes deny even loopback sockets.
            return;
        };
        let port = listener.local_addr().unwrap().port();
        assert_eq!(probe_port(port), PortProbe::Listening);
        drop(listener);
        assert_eq!(probe_port(port), PortProbe::Closed);
    }

    #[test]
    fn process_tree_identity_accepts_the_root_process() {
        let pid = std::process::id();
        assert!(belongs_to_process_tree(pid, pid));
    }

    #[test]
    fn host_probe_supports_explicit_ipv4_and_ipv6() {
        let Ok(ipv4) = TcpListener::bind(("127.0.0.1", 0)) else {
            return;
        };
        assert_eq!(
            probe_host_port(
                "127.0.0.1",
                ipv4.local_addr().unwrap().port(),
                Duration::from_millis(100)
            ),
            PortProbe::Listening
        );
        if let Ok(ipv6) = TcpListener::bind(("::1", 0)) {
            assert_eq!(
                probe_host_port(
                    "::1",
                    ipv6.local_addr().unwrap().port(),
                    Duration::from_millis(100)
                ),
                PortProbe::Listening
            );
        }
    }
}
