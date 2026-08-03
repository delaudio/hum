use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
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
        format!("{}  {}", self.timestamp.format("%H:%M:%S"), self.content)
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
const FOLLOW_READ_CHUNK: u64 = 64 * 1024;
const MAX_PARTIAL_LINE: usize = 64 * 1024;

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

pub fn tail_file(path: &Path, count: usize) -> Result<Vec<String>> {
    if count == 0 || !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut tail = VecDeque::with_capacity(count.min(1024));
    for line in BufReader::new(file).lines() {
        if tail.len() == count {
            tail.pop_front();
        }
        tail.push_back(line?);
    }
    Ok(tail.into_iter().collect())
}

pub struct FileFollower {
    file: File,
    offset: u64,
    partial: String,
}

impl FileFollower {
    pub fn from_end(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let mut file =
            File::open(path).with_context(|| format!("failed to follow {}", path.display()))?;
        let offset = file.seek(SeekFrom::End(0))?;
        Ok(Some(Self {
            file,
            offset,
            partial: String::new(),
        }))
    }

    pub fn read_new_lines(&mut self) -> Result<Vec<String>> {
        let length = self.file.metadata()?.len();
        if length < self.offset {
            self.file.seek(SeekFrom::Start(0))?;
            self.offset = 0;
            self.partial.clear();
        } else {
            self.file.seek(SeekFrom::Start(self.offset))?;
        }
        let mut bytes = Vec::with_capacity(FOLLOW_READ_CHUNK as usize);
        (&mut self.file)
            .take(FOLLOW_READ_CHUNK)
            .read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&bytes));

        let mut lines = Vec::new();
        while let Some(newline) = self.partial.find('\n') {
            let line = self.partial.drain(..=newline).collect::<String>();
            lines.push(line.trim_end_matches(['\r', '\n']).to_string());
        }
        while self.partial.len() > MAX_PARTIAL_LINE {
            let mut boundary = MAX_PARTIAL_LINE;
            while !self.partial.is_char_boundary(boundary) {
                boundary -= 1;
            }
            let chunk = self.partial.drain(..boundary).collect::<String>();
            lines.push(format!("{chunk}… [continued]"));
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn follower_bounds_memory_for_output_without_newlines() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hum-log-follow-{}-{unique}.log",
            std::process::id()
        ));
        File::create(&path).unwrap();
        let mut follower = FileFollower::from_end(&path).unwrap().unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writer.write_all(&vec![b'x'; MAX_PARTIAL_LINE * 3]).unwrap();
        writer.flush().unwrap();

        let mut emitted = Vec::new();
        for _ in 0..3 {
            emitted.extend(follower.read_new_lines().unwrap());
            assert!(follower.partial.len() <= MAX_PARTIAL_LINE);
        }
        assert!(!emitted.is_empty());
        assert!(emitted.iter().all(|line| line.ends_with("… [continued]")));
        std::fs::remove_file(path).unwrap();
    }
}
