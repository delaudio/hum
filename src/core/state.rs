use chrono::{DateTime, Utc};
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

    pub fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PortState {
    Listening,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceState {
    pub process: ProcessState,
    pub port: PortState,
    pub health: HealthState,
    pub generation: u64,
    pub exit_code: Option<i32>,
    pub changed_at: DateTime<Utc>,
    pub last_health_at: Option<DateTime<Utc>>,
    pub last_health_duration_ms: Option<u64>,
    pub health_detail: Option<String>,
    pub last_error: Option<String>,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            process: ProcessState::Missing,
            port: PortState::Unknown,
            health: HealthState::Unchecked,
            generation: 0,
            exit_code: None,
            changed_at: Utc::now(),
            last_health_at: None,
            last_health_duration_ms: None,
            health_detail: None,
            last_error: None,
        }
    }
}

impl ServiceState {
    pub fn presentation(&self) -> PresentationState {
        match self.process {
            ProcessState::Starting => PresentationState::Starting,
            ProcessState::Stopping => PresentationState::Stopping,
            ProcessState::Exited => PresentationState::Exited,
            ProcessState::Missing if self.last_error.is_some() => PresentationState::Blocked,
            ProcessState::Missing => PresentationState::Missing,
            ProcessState::Running if self.health == HealthState::Unhealthy => {
                PresentationState::Degraded
            }
            ProcessState::Running
                if self.health == HealthState::Healthy
                    || (self.health == HealthState::Unchecked
                        && self.port == PortState::Listening) =>
            {
                PresentationState::Ready
            }
            ProcessState::Running => PresentationState::Running,
        }
    }

    pub fn begin_start(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.process = ProcessState::Starting;
        self.health = HealthState::Unchecked;
        self.exit_code = None;
        self.health_detail = None;
        self.last_health_at = None;
        self.last_health_duration_ms = None;
        self.last_error = None;
        self.changed_at = Utc::now();
        self.generation
    }

    pub fn mark_running(&mut self, generation: u64, has_healthcheck: bool) -> bool {
        if self.generation != generation || self.process != ProcessState::Starting {
            return false;
        }
        self.process = ProcessState::Running;
        self.health = if has_healthcheck {
            HealthState::Checking
        } else {
            HealthState::Unchecked
        };
        self.changed_at = Utc::now();
        true
    }

    pub fn begin_stop(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.process = ProcessState::Stopping;
        self.health = HealthState::Unchecked;
        self.health_detail = None;
        self.last_health_at = None;
        self.last_health_duration_ms = None;
        self.changed_at = Utc::now();
        self.generation
    }

    pub fn mark_missing(&mut self) {
        self.process = ProcessState::Missing;
        self.health = HealthState::Unchecked;
        self.health_detail = None;
        self.last_health_at = None;
        self.last_health_duration_ms = None;
        self.changed_at = Utc::now();
    }

    pub fn mark_exited(&mut self, exit_code: Option<i32>, error: impl Into<String>) {
        self.process = ProcessState::Exited;
        self.health = HealthState::Unchecked;
        self.exit_code = exit_code;
        self.health_detail = None;
        self.last_health_at = None;
        self.last_health_duration_ms = None;
        self.last_error = Some(error.into());
        self.changed_at = Utc::now();
    }

    pub fn mark_exited_for_generation(
        &mut self,
        generation: u64,
        exit_code: Option<i32>,
        error: impl Into<String>,
    ) -> bool {
        if self.generation != generation || !self.process.is_active() {
            return false;
        }
        self.mark_exited(exit_code, error);
        true
    }

    pub fn mark_start_failed(&mut self, error: impl Into<String>) {
        self.process = ProcessState::Missing;
        self.health = HealthState::Unchecked;
        self.last_error = Some(error.into());
        self.changed_at = Utc::now();
    }

    pub fn apply_health(
        &mut self,
        generation: u64,
        health: HealthState,
        detail: impl Into<String>,
        duration_ms: u64,
    ) -> bool {
        if self.generation != generation || self.process != ProcessState::Running {
            return false;
        }
        self.health = health;
        self.health_detail = Some(detail.into());
        self.last_health_at = Some(Utc::now());
        self.last_health_duration_ms = Some(duration_ms);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_process_can_be_unhealthy_without_losing_process_state() {
        let mut state = ServiceState::default();
        let generation = state.begin_start();
        assert!(state.mark_running(generation, true));
        assert!(state.apply_health(generation, HealthState::Unhealthy, "timeout", 42));
        assert_eq!(state.process, ProcessState::Running);
        assert_eq!(state.health, HealthState::Unhealthy);
        assert_eq!(state.last_health_duration_ms, Some(42));
        assert_eq!(state.presentation(), PresentationState::Degraded);
    }

    #[test]
    fn exited_process_rejects_health_results() {
        let mut state = ServiceState::default();
        let generation = state.begin_start();
        state.mark_running(generation, true);
        state.mark_exited(Some(1), "process exited");
        assert!(!state.apply_health(generation, HealthState::Healthy, "ok", 1));
        assert_eq!(state.process, ProcessState::Exited);
        assert_eq!(state.health, HealthState::Unchecked);
        assert_eq!(state.exit_code, Some(1));
        assert_eq!(state.last_error.as_deref(), Some("process exited"));
    }

    #[test]
    fn restart_invalidates_previous_generation() {
        let mut state = ServiceState::default();
        let first = state.begin_start();
        state.mark_running(first, true);
        state.begin_stop();
        state.mark_missing();
        let second = state.begin_start();
        state.mark_running(second, true);

        assert_ne!(first, second);
        assert!(!state.apply_health(first, HealthState::Healthy, "stale", 1));
        assert!(!state.mark_exited_for_generation(first, Some(1), "stale exit"));
        assert_eq!(state.process, ProcessState::Running);
        assert_eq!(state.health, HealthState::Checking);
    }
}
