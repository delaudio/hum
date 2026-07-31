use std::net::{SocketAddr, TcpListener};

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
    Some(find_occupant(port))
}

fn find_occupant(port: u16) -> PortOccupant {
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
