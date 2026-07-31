use serde::Serialize;

/// RF-10: process status is distinct from application health (section 16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ServiceStatus {
    Stopped,
    Starting,
    Running,
    Healthy,
    Unhealthy,
    Stopping,
    Failed,
    Blocked,
}

impl ServiceStatus {
    pub fn label(&self) -> &'static str {
        match self {
            ServiceStatus::Stopped => "stopped",
            ServiceStatus::Starting => "starting",
            ServiceStatus::Running => "running",
            ServiceStatus::Healthy => "healthy",
            ServiceStatus::Unhealthy => "unhealthy",
            ServiceStatus::Stopping => "stopping",
            ServiceStatus::Failed => "failed",
            ServiceStatus::Blocked => "blocked",
        }
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            ServiceStatus::Stopped => "○",
            ServiceStatus::Starting => "◐",
            ServiceStatus::Running => "●",
            ServiceStatus::Healthy => "●",
            ServiceStatus::Unhealthy => "●",
            ServiceStatus::Stopping => "◐",
            ServiceStatus::Failed => "✗",
            ServiceStatus::Blocked => "!",
        }
    }

    /// Whether the process is alive in some form (used to decide whether a
    /// dependent's "started" ready-mode is satisfied).
    pub fn is_started(&self) -> bool {
        matches!(
            self,
            ServiceStatus::Running | ServiceStatus::Healthy | ServiceStatus::Unhealthy
        )
    }

    #[allow(dead_code)]
    pub fn is_healthy_or_no_check(&self) -> bool {
        matches!(self, ServiceStatus::Healthy)
    }

    #[allow(dead_code)]
    pub fn is_terminal_failure(&self) -> bool {
        matches!(self, ServiceStatus::Failed | ServiceStatus::Blocked)
    }
}
