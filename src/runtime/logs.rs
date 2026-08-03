use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
const FOLLOW_READ_CHUNK: u64 = 64 * 1024;
const MAX_PARTIAL_LINE: usize = 64 * 1024;
const TAIL_READ_CHUNK: usize = 64 * 1024;
const MAX_TAIL_BYTES: usize = 512 * 1024;

pub fn tail_file(path: &Path, count: usize) -> Result<Vec<String>> {
    if count == 0 || !path.exists() {
        return Ok(Vec::new());
    }
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut position = file.metadata()?.len();
    let mut chunks = VecDeque::new();
    let mut bytes_read = 0;
    let mut newlines = 0;
    while position > 0 && bytes_read < MAX_TAIL_BYTES && newlines <= count {
        let amount = (position as usize)
            .min(TAIL_READ_CHUNK)
            .min(MAX_TAIL_BYTES - bytes_read);
        position -= amount as u64;
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = vec![0; amount];
        file.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
        bytes_read += amount;
        chunks.push_front(chunk);
    }
    let bytes = chunks.into_iter().flatten().collect::<Vec<_>>();
    let content = String::from_utf8_lossy(&bytes);
    let mut lines = content
        .lines()
        .rev()
        .take(count)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    lines.reverse();
    if position > 0 && newlines <= count {
        if let Some(first) = lines.first_mut() {
            first.insert_str(0, "… [tail truncated] ");
        }
    }
    Ok(lines)
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

    #[test]
    fn tail_reads_from_the_end_with_a_byte_limit() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("hum-log-tail-{}-{unique}.log", std::process::id()));
        let mut file = File::create(&path).unwrap();
        file.write_all(b"first\nsecond\nthird\n").unwrap();
        assert_eq!(tail_file(&path, 2).unwrap(), ["second", "third"]);

        file.set_len(0).unwrap();
        file.write_all(&vec![b'x'; MAX_TAIL_BYTES * 2]).unwrap();
        let tail = tail_file(&path, 2).unwrap();
        assert_eq!(tail.len(), 1);
        assert!(tail[0].starts_with("… [tail truncated] "));
        assert!(tail[0].len() <= MAX_TAIL_BYTES + 32);
        std::fs::remove_file(path).unwrap();
    }
}
