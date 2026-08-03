use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessState {
    Starting,
    Running,
    Exited,
    Missing,
    Stopping,
}

impl ProcessState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Missing => "missing",
            Self::Stopping => "stopping",
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Starting | Self::Stopping => "◐",
            Self::Running => "●",
            Self::Exited => "✗",
            Self::Missing => "○",
        }
    }

    pub fn is_running(self) -> bool {
        self == Self::Running
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PortState {
    Listening,
    ListeningUnverified,
    Closed,
    Unknown,
    OccupiedByOther {
        pid: Option<u32>,
        process_name: Option<String>,
    },
}

impl PortState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Listening => "listening",
            Self::ListeningUnverified => "listening-unverified",
            Self::Closed => "closed",
            Self::Unknown => "unknown",
            Self::OccupiedByOther { .. } => "occupied-by-other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HealthState {
    Unchecked,
    Checking,
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresentationState {
    Starting,
    Running,
    Ready,
    Degraded,
    Stopping,
    Exited,
    Missing,
    Blocked,
}

impl PresentationState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Stopping => "stopping",
            Self::Exited => "exited",
            Self::Missing => "missing",
            Self::Blocked => "blocked",
        }
    }
}

impl HealthState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unchecked => "unchecked",
            Self::Checking => "checking",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
        }
    }
}
