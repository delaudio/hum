use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProbe {
    Listening,
    Closed,
    Unknown,
}

/// Lightweight port polling: one bounded TCP connection, with no process
/// enumeration and no `lsof` subprocess.
pub fn probe_port(port: u16) -> PortProbe {
    let address: SocketAddr = ([127, 0, 0, 1], port).into();
    match TcpStream::connect_timeout(&address, Duration::from_millis(50)) {
        Ok(_) => PortProbe::Listening,
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => PortProbe::Closed,
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => PortProbe::Unknown,
        Err(_) => PortProbe::Closed,
    }
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
    use sysinfo::System;

    let mut system = System::new_all();
    system.refresh_all();

    // Best-effort: sysinfo does not expose per-socket ownership portably, so
    // we shell out to `lsof` on unix, which is present on macOS and most
    // Linux dev boxes, and fall back to "unknown" if it isn't.
    #[cfg(unix)]
    {
        if let Ok(output) = std::process::Command::new("lsof")
            .args(["-i", &format!(":{port}"), "-sTCP:LISTEN", "-t"])
            .output()
        {
            if let Ok(text) = String::from_utf8(output.stdout) {
                if let Some(pid_str) = text.lines().next() {
                    if let Ok(pid) = pid_str.trim().parse::<u32>() {
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
}
