use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local};

/// RF-11: which stream a captured line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Stream {
    Stdout,
    Stderr,
    /// Lines emitted by `hum` itself about the service (start/stop/crash).
    System,
}

impl Stream {
    pub fn label(&self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
            Stream::System => "system",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LogLine {
    pub timestamp: DateTime<Local>,
    pub service: String,
    pub stream: Stream,
    pub content: String,
}

impl LogLine {
    #[allow(dead_code)]
    pub fn format(&self) -> String {
        format!(
            "{}  {}",
            self.timestamp.format("%H:%M:%S"),
            self.content
        )
    }
}

/// RF-12: a circular buffer of log lines for a single service. Bounded so
/// long-running services don't grow memory without limit.
#[derive(Debug)]
pub struct LogBuffer {
    capacity: usize,
    lines: Mutex<VecDeque<LogLine>>,
}

pub const DEFAULT_CAPACITY: usize = 10_000;

impl LogBuffer {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(LogBuffer {
            capacity,
            lines: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
        })
    }

    pub fn push(&self, line: LogLine) {
        let mut lines = self.lines.lock().unwrap();
        if lines.len() >= self.capacity {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    #[allow(dead_code)]
    pub fn snapshot(&self) -> Vec<LogLine> {
        self.lines.lock().unwrap().iter().cloned().collect()
    }

    #[allow(dead_code)]
    pub fn clear(&self) {
        self.lines.lock().unwrap().clear();
    }

    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let lines = self.lines.lock().unwrap();
        let len = lines.len();
        let skip = len.saturating_sub(n);
        lines.iter().skip(skip).cloned().collect()
    }
}
