use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::LogConfig;

pub const INTERNAL_SINK_ARG: &str = "__log-sink";
const FOLLOW_READ_CHUNK: usize = 64 * 1024;
const TAIL_READ_CHUNK: usize = 64 * 1024;
const MAX_TAIL_BYTES: usize = 512 * 1024;
const BOUNDARY_MARKER_BYTES: u64 = 17;

#[derive(Debug, Clone, Copy)]
pub struct LogPolicy {
    pub max_file_bytes: u64,
    pub rotated_files: usize,
    pub retention: Option<Duration>,
}

impl From<&LogConfig> for LogPolicy {
    fn from(config: &LogConfig) -> Self {
        Self {
            max_file_bytes: config.max_file_bytes,
            rotated_files: config.rotated_files,
            retention: config.retention,
        }
    }
}

pub struct Redactor {
    patterns: Vec<regex::Regex>,
}

impl Redactor {
    pub fn new(patterns: &[String]) -> Result<Self> {
        let patterns = patterns
            .iter()
            .map(|pattern| regex::Regex::new(pattern).map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { patterns })
    }

    pub fn redact(&self, value: &str) -> String {
        self.patterns
            .iter()
            .fold(value.to_string(), |redacted, pattern| {
                pattern.replace_all(&redacted, "[REDACTED]").into_owned()
            })
    }

    pub fn redact_bounded(&self, value: &str, max_bytes: usize) -> String {
        if value.len() > max_bytes {
            "… [oversized log line omitted]".to_string()
        } else {
            self.redact(value)
        }
    }
}

#[cfg(not(test))]
pub fn internal_sink_args(
    stdout_path: &Path,
    stderr_path: &Path,
    policy: LogPolicy,
) -> Vec<OsString> {
    vec![
        OsString::from(INTERNAL_SINK_ARG),
        stdout_path.as_os_str().to_owned(),
        stderr_path.as_os_str().to_owned(),
        OsString::from(policy.max_file_bytes.to_string()),
        OsString::from(policy.rotated_files.to_string()),
        OsString::from(
            policy
                .retention
                .map(|duration| duration.as_millis().min(u128::from(u64::MAX)).to_string())
                .unwrap_or_else(|| "-".to_string()),
        ),
    ]
}

/// Handle the private log-sink process before Clap and Tokio are initialized.
pub fn try_run_internal_sink(args: &[OsString]) -> Option<i32> {
    if args.get(1).and_then(|arg| arg.to_str()) != Some(INTERNAL_SINK_ARG) {
        return None;
    }
    Some(
        match parse_internal_sink_args(args).and_then(run_internal_sink) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("hum internal log sink failed: {error:#}");
                1
            }
        },
    )
}

fn parse_internal_sink_args(args: &[OsString]) -> Result<(PathBuf, PathBuf, LogPolicy)> {
    if args.len() != 7 {
        anyhow::bail!("invalid internal log sink arguments");
    }
    let parse = |index: usize, label: &str| -> Result<u64> {
        args[index]
            .to_str()
            .and_then(|value| value.parse().ok())
            .with_context(|| format!("invalid {label}"))
    };
    let retention = match args[6].to_str() {
        Some("-") => None,
        Some(value) => Some(Duration::from_millis(
            value.parse().context("invalid retention")?,
        )),
        None => anyhow::bail!("invalid retention"),
    };
    let max_file_bytes = parse(4, "maximum file size")?;
    let rotated_files = usize::try_from(parse(5, "rotated file count")?)?;
    if max_file_bytes == 0 || rotated_files > 16 {
        anyhow::bail!("invalid internal log rotation policy");
    }
    Ok((
        PathBuf::from(&args[2]),
        PathBuf::from(&args[3]),
        LogPolicy {
            max_file_bytes,
            rotated_files,
            retention,
        },
    ))
}

#[cfg(unix)]
fn run_internal_sink(
    (stdout_path, stderr_path, policy): (PathBuf, PathBuf, LogPolicy),
) -> Result<()> {
    use std::os::fd::FromRawFd;

    // SAFETY: this private process is started with stream pipes on fd 0 and 3,
    // and takes sole ownership of both descriptors.
    let mut stdout = unsafe { File::from_raw_fd(0) };
    let mut stderr = unsafe { File::from_raw_fd(3) };
    pump_streams(&mut stdout, &mut stderr, stdout_path, stderr_path, policy)
}

#[cfg(all(test, unix))]
pub fn spawn_test_sink(
    stdout: std::os::unix::net::UnixStream,
    stderr: std::os::unix::net::UnixStream,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    policy: LogPolicy,
) -> std::thread::JoinHandle<()> {
    use std::os::fd::OwnedFd;

    std::thread::spawn(move || {
        let mut stdout = File::from(OwnedFd::from(stdout));
        let mut stderr = File::from(OwnedFd::from(stderr));
        let _ = pump_streams(&mut stdout, &mut stderr, stdout_path, stderr_path, policy);
    })
}

#[cfg(unix)]
fn pump_streams(
    stdout: &mut File,
    stderr: &mut File,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    policy: LogPolicy,
) -> Result<()> {
    use std::os::fd::AsRawFd;

    let mut stdout_writer = RotatingWriter::new(stdout_path, policy).ok();
    let mut stderr_writer = RotatingWriter::new(stderr_path, policy).ok();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut buffer = vec![0_u8; FOLLOW_READ_CHUNK];

    while stdout_open || stderr_open {
        let mut descriptors = [
            libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: if stdout_open { libc::POLLIN } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: stderr.as_raw_fd(),
                events: if stderr_open { libc::POLLIN } else { 0 },
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if stdout_open && descriptors[0].revents != 0 {
            stdout_open = drain_once(stdout, &mut stdout_writer, &mut buffer)?;
        }
        if stderr_open && descriptors[1].revents != 0 {
            stderr_open = drain_once(stderr, &mut stderr_writer, &mut buffer)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn run_internal_sink(_: (PathBuf, PathBuf, LogPolicy)) -> Result<()> {
    anyhow::bail!("the internal log sink is supported on Unix platforms")
}

fn drain_once(
    reader: &mut File,
    writer: &mut Option<RotatingWriter>,
    buffer: &mut [u8],
) -> Result<bool> {
    match reader.read(buffer) {
        Ok(0) => Ok(false),
        Ok(count) => {
            if let Some(active) = writer {
                // Disk errors must not close the reader and deliver SIGPIPE to
                // a noisy service. Disable persistence but keep draining.
                if active.write_bounded(&buffer[..count]).is_err() {
                    *writer = None;
                }
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => Ok(true),
        Err(error) => Err(error.into()),
    }
}

struct RotatingWriter {
    path: PathBuf,
    file: File,
    bytes: u64,
    last_byte_was_newline: bool,
    policy: LogPolicy,
}

impl RotatingWriter {
    fn new(path: PathBuf, policy: LogPolicy) -> Result<Self> {
        cleanup_rotated(&path, policy)?;
        let last_byte_was_newline = file_ends_at_line_boundary(&path)?;
        let file = open_log(&path, true)?;
        let bytes = file.metadata()?.len();
        if bytes == 0 && !boundary_path(&path).exists() {
            write_boundary_marker(&path, true)?;
        }
        let mut writer = Self {
            path,
            file,
            bytes,
            last_byte_was_newline,
            policy,
        };
        if writer.bytes >= writer.policy.max_file_bytes {
            writer.rotate()?;
        }
        Ok(writer)
    }

    fn write_bounded(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            if self.bytes >= self.policy.max_file_bytes {
                self.rotate()?;
            }
            let available =
                usize::try_from(self.policy.max_file_bytes - self.bytes).unwrap_or(usize::MAX);
            let count = available.min(bytes.len());
            self.file.write_all(&bytes[..count])?;
            self.file.flush()?;
            self.bytes += count as u64;
            self.last_byte_was_newline = bytes[count - 1] == b'\n';
            bytes = &bytes[count..];
        }
        Ok(())
    }

    fn rotate(&mut self) -> Result<()> {
        self.file.flush()?;
        let next_starts_at_boundary = self.last_byte_was_newline;
        if self.policy.rotated_files == 0 {
            self.file = replace_log_file(&self.path)?;
            self.bytes = 0;
            write_boundary_marker(&self.path, next_starts_at_boundary)?;
            return Ok(());
        }

        let oldest = rotated_path(&self.path, self.policy.rotated_files);
        remove_if_exists(&oldest)?;
        remove_if_exists(&boundary_path(&oldest))?;
        for index in (1..self.policy.rotated_files).rev() {
            let source = rotated_path(&self.path, index);
            if source.exists() {
                let destination = rotated_path(&self.path, index + 1);
                remove_if_exists(&destination)?;
                fs::rename(source, destination)?;
                move_boundary_marker(
                    &rotated_path(&self.path, index),
                    &rotated_path(&self.path, index + 1),
                )?;
            }
        }
        if self.path.exists() {
            fs::rename(&self.path, rotated_path(&self.path, 1))?;
            move_boundary_marker(&self.path, &rotated_path(&self.path, 1))?;
        }
        self.file = open_log_truncated(&self.path)?;
        self.bytes = 0;
        write_boundary_marker(&self.path, next_starts_at_boundary)?;
        cleanup_rotated(&self.path, self.policy)?;
        Ok(())
    }
}

fn open_log(path: &Path, append: bool) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(append)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    restrict_log_file(&file)?;
    Ok(file)
}

fn open_log_truncated(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    restrict_log_file(&file)?;
    Ok(file)
}

fn replace_log_file(path: &Path) -> Result<File> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".replace.{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    remove_if_exists(&temporary)?;
    drop(open_log_truncated(&temporary)?);
    fs::rename(&temporary, path)?;
    open_log(path, true)
}

fn cleanup_rotated(path: &Path, policy: LogPolicy) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(base) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(index) = name
            .to_str()
            .and_then(|name| name.strip_prefix(&format!("{base}.")))
            .and_then(|suffix| suffix.parse::<usize>().ok())
        else {
            continue;
        };
        let expired = policy.retention.is_some_and(|retention| {
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= retention)
        });
        if index > policy.rotated_files || expired {
            fs::remove_file(entry.path())?;
            remove_if_exists(&boundary_path(&entry.path()))?;
        } else {
            retain_file_suffix(&entry.path(), policy.max_file_bytes)?;
        }
    }
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_log_temporary(&name, base) {
            fs::remove_file(entry.path())?;
            continue;
        }
        let Some(index) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix(&format!("{base}.")))
            .and_then(|suffix| suffix.strip_suffix(".boundary"))
            .and_then(|suffix| suffix.parse::<usize>().ok())
        else {
            continue;
        };
        if index > policy.rotated_files || !rotated_path(path, index).exists() {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn is_log_temporary(name: &str, base: &str) -> bool {
    let Some(suffix) = name.strip_prefix(base) else {
        return false;
    };
    if let Some(pid) = suffix.strip_prefix(".replace.") {
        return !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit());
    }
    let Some((generation, pid)) = suffix
        .strip_prefix('.')
        .and_then(|suffix| suffix.split_once(".bound."))
    else {
        return false;
    };
    !generation.is_empty()
        && generation.bytes().all(|byte| byte.is_ascii_digit())
        && !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
}

fn retain_file_suffix(path: &Path, max_bytes: u64) -> Result<()> {
    let mut source = File::open(path)?;
    let length = source.metadata()?.len();
    if length <= max_bytes {
        return Ok(());
    }

    source.seek(SeekFrom::Start(length - max_bytes))?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".bound.{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    remove_if_exists(&temporary)?;
    let mut destination = open_log_truncated(&temporary)?;
    std::io::copy(&mut source.take(max_bytes), &mut destination)?;
    destination.flush()?;
    drop(destination);
    fs::rename(&temporary, path)?;
    write_boundary_marker(path, false)?;
    Ok(())
}

fn file_ends_at_line_boundary(path: &Path) -> Result<bool> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    Ok(byte[0] == b'\n')
}

fn boundary_path(path: &Path) -> PathBuf {
    let mut marker = path.as_os_str().to_owned();
    marker.push(".boundary");
    PathBuf::from(marker)
}

fn write_boundary_marker(path: &Path, verified: bool) -> Result<()> {
    let identity = file_identity(&fs::metadata(path)?);
    let mut marker = open_log_truncated(&boundary_path(path))?;
    marker.write_all(&identity.first.to_le_bytes())?;
    marker.write_all(&identity.second.to_le_bytes())?;
    marker.write_all(&[u8::from(verified)])?;
    marker.flush()?;
    Ok(())
}

fn read_boundary_marker(path: &Path, expected: FileIdentity) -> bool {
    fs::read(boundary_path(path)).is_ok_and(|value| {
        if value.len() != BOUNDARY_MARKER_BYTES as usize {
            return false;
        }
        let first = u64::from_le_bytes(value[0..8].try_into().unwrap());
        let second = u64::from_le_bytes(value[8..16].try_into().unwrap());
        FileIdentity { first, second } == expected && value[16] == 1
    })
}

fn move_boundary_marker(source: &Path, destination: &Path) -> Result<()> {
    let source = boundary_path(source);
    let destination = boundary_path(destination);
    remove_if_exists(&destination)?;
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn restrict_log_file(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_log_file(_file: &File) -> Result<()> {
    Ok(())
}

pub fn tail_history(path: &Path, count: usize, rotated_files: usize) -> Result<Vec<String>> {
    let sources = (0..=rotated_files)
        .map(|index| {
            if index == 0 {
                path.to_path_buf()
            } else {
                rotated_path(path, index)
            }
        })
        .collect::<Vec<_>>();
    tail_sources(&sources, count)
}

/// A bounded, backwards reader over a snapshot of an active log and its rotations.
///
/// Each call advances towards older data without retaining the previously read
/// history in memory. Open file handles keep the snapshot stable while the
/// active writer continues rotating files.
pub struct HistoryPager {
    sources: Vec<HistorySource>,
    source_index: usize,
    reversed_line: Vec<u8>,
    oversized_line: bool,
    discard_newest_fragment: bool,
    skip_trailing_separator: bool,
    max_line_bytes: usize,
    finished: bool,
    saw_content: bool,
}

struct HistorySource {
    file: File,
    position: u64,
    start_verified: bool,
}

impl HistoryPager {
    pub fn open(path: &Path, rotated_files: usize, max_line_bytes: usize) -> Result<Self> {
        let sources = open_history_sources(path, rotated_files, None)?;
        Self::from_sources(sources, max_line_bytes)
    }

    fn from_sources(sources: Vec<HistorySource>, max_line_bytes: usize) -> Result<Self> {
        let discard_newest_fragment = match sources.iter().find(|source| source.position > 0) {
            Some(source) => {
                let mut file = source.file.try_clone()?;
                file.seek(SeekFrom::Start(source.position - 1))?;
                let mut byte = [0];
                file.read_exact(&mut byte)?;
                byte[0] != b'\n'
            }
            _ => false,
        };
        Ok(Self {
            sources,
            source_index: 0,
            reversed_line: Vec::new(),
            oversized_line: false,
            discard_newest_fragment,
            skip_trailing_separator: true,
            max_line_bytes,
            finished: false,
            saw_content: false,
        })
    }

    /// Return the next older page in chronological order.
    ///
    /// Disk work per call is capped so a noisy or newline-free log cannot stall
    /// the TUI. If a page is empty while `has_more` is true, another call keeps
    /// advancing through that bounded input.
    pub fn next_older(&mut self, count: usize) -> Result<Vec<String>> {
        if count == 0 || self.finished {
            return Ok(Vec::new());
        }
        let mut newest_first = Vec::new();
        let mut bytes_read = 0;
        while newest_first.len() < count
            && bytes_read < MAX_TAIL_BYTES
            && self.source_index < self.sources.len()
        {
            if self.sources[self.source_index].position == 0 {
                self.source_index += 1;
                continue;
            }
            let amount = (self.sources[self.source_index].position as usize)
                .min(TAIL_READ_CHUNK)
                .min(MAX_TAIL_BYTES - bytes_read);
            let end = self.sources[self.source_index].position;
            let start = end - amount as u64;
            let mut chunk = vec![0; amount];
            {
                let source = &mut self.sources[self.source_index];
                source.file.seek(SeekFrom::Start(start))?;
                source.file.read_exact(&mut chunk)?;
            }

            let mut consumed = 0_u64;
            for byte in chunk.into_iter().rev() {
                consumed += 1;
                bytes_read += 1;
                self.consume_byte(byte, &mut newest_first);
                if newest_first.len() == count || bytes_read == MAX_TAIL_BYTES {
                    break;
                }
            }
            self.sources[self.source_index].position = end - consumed;
        }

        if self.source_index == self.sources.len() && !self.finished {
            self.finish_history(&mut newest_first);
        }
        newest_first.reverse();
        Ok(newest_first)
    }

    pub fn has_more(&self) -> bool {
        !self.finished
    }

    fn consume_byte(&mut self, byte: u8, lines: &mut Vec<String>) {
        self.saw_content = true;
        if self.discard_newest_fragment {
            if byte == b'\n' {
                self.discard_newest_fragment = false;
                self.skip_trailing_separator = false;
                lines.push("… [incomplete log line omitted]".to_string());
            }
            return;
        }
        if byte == b'\n' {
            if self.skip_trailing_separator {
                self.skip_trailing_separator = false;
            } else {
                lines.push(self.take_line());
            }
            return;
        }
        self.skip_trailing_separator = false;
        if self.reversed_line.len() < self.max_line_bytes {
            self.reversed_line.push(byte);
        } else {
            self.oversized_line = true;
        }
    }

    fn take_line(&mut self) -> String {
        if self.oversized_line {
            self.reversed_line.clear();
            self.oversized_line = false;
            return "… [oversized log line omitted]".to_string();
        }
        self.reversed_line.reverse();
        let line = String::from_utf8_lossy(&self.reversed_line).into_owned();
        self.reversed_line.clear();
        line
    }

    fn finish_history(&mut self, lines: &mut Vec<String>) {
        let discarded_at_eof = self.discard_newest_fragment && self.saw_content;
        if discarded_at_eof {
            lines.push("… [incomplete log line omitted]".to_string());
            self.discard_newest_fragment = false;
        }
        let oldest_verified = self
            .sources
            .last()
            .is_some_and(|source| source.start_verified);
        if !self.discard_newest_fragment && (!self.reversed_line.is_empty() || self.oversized_line)
        {
            if oldest_verified {
                lines.push(self.take_line());
            } else {
                self.reversed_line.clear();
                self.oversized_line = false;
                lines.push("… [history boundary]".to_string());
            }
        } else if self.saw_content
            && !oldest_verified
            && !self.discard_newest_fragment
            && !discarded_at_eof
        {
            lines.push("… [history boundary]".to_string());
        }
        self.finished = true;
    }
}

fn open_history_sources(
    path: &Path,
    rotated_files: usize,
    active: Option<(&File, u64, FileIdentity)>,
) -> Result<Vec<HistorySource>> {
    let mut sources = Vec::new();
    let mut identities = std::collections::HashSet::new();
    if let Some((file, length, identity)) = active {
        identities.insert(identity);
        sources.push(HistorySource {
            file: file.try_clone()?,
            position: length,
            start_verified: read_boundary_marker(path, identity),
        });
    } else {
        match File::open(path) {
            Ok(file) => {
                let metadata = file.metadata()?;
                let identity = file_identity(&metadata);
                identities.insert(identity);
                sources.push(HistorySource {
                    file,
                    position: metadata.len(),
                    start_verified: read_boundary_marker(path, identity),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to open {}", path.display()));
            }
        }
    }
    for index in 1..=rotated_files {
        let rotated = rotated_path(path, index);
        let file = match File::open(&rotated) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to open {}", rotated.display()));
            }
        };
        let metadata = file.metadata()?;
        let identity = file_identity(&metadata);
        if identities.insert(identity) {
            sources.push(HistorySource {
                file,
                position: metadata.len(),
                start_verified: read_boundary_marker(&rotated, identity),
            });
        }
    }
    Ok(sources)
}

fn tail_sources(sources_newest_first: &[PathBuf], count: usize) -> Result<Vec<String>> {
    let mut sources = Vec::new();
    for source in sources_newest_first {
        let file = match File::open(source) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to open {}", source.display()));
            }
        };
        let metadata = file.metadata()?;
        let length = metadata.len();
        let identity = file_identity(&metadata);
        sources.push(OpenTailSource {
            file,
            length,
            start_verified: read_boundary_marker(source, identity),
        });
    }
    tail_open_sources(&mut sources, count)
}

struct OpenTailSource {
    file: File,
    length: u64,
    start_verified: bool,
}

fn tail_open_sources(
    sources_newest_first: &mut [OpenTailSource],
    count: usize,
) -> Result<Vec<String>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut chunks = VecDeque::new();
    let mut bytes_read = 0;
    let mut newlines = 0;
    let mut truncated = false;
    let mut oldest_start_verified = false;
    for source in sources_newest_first {
        if bytes_read >= MAX_TAIL_BYTES || newlines > count {
            break;
        }
        oldest_start_verified = source.start_verified;
        let mut position = source.length;
        while position > 0 && bytes_read < MAX_TAIL_BYTES && newlines <= count {
            let amount = (position as usize)
                .min(TAIL_READ_CHUNK)
                .min(MAX_TAIL_BYTES - bytes_read);
            position -= amount as u64;
            source.file.seek(SeekFrom::Start(position))?;
            let mut chunk = vec![0; amount];
            source.file.read_exact(&mut chunk)?;
            newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
            bytes_read += amount;
            chunks.push_front(chunk);
        }
        if position > 0 {
            truncated = true;
            oldest_start_verified = false;
        }
    }
    let bytes = chunks.into_iter().flatten().collect::<Vec<_>>();
    let incomplete_last_line = bytes.last().is_some_and(|byte| *byte != b'\n');
    let content = String::from_utf8_lossy(&bytes);
    let mut lines = content
        .lines()
        .rev()
        .take(count.saturating_add(usize::from(incomplete_last_line)))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    lines.reverse();
    if incomplete_last_line {
        lines.pop();
    }
    if newlines <= count && !oldest_start_verified && !lines.is_empty() {
        // When the complete retained history fits in the requested tail, its
        // first bytes have no independently verifiable stream boundary. The
        // active file may itself begin mid-line when history retention is
        // disabled. Never display that fragment: it could bypass redaction.
        lines.remove(0);
        lines.insert(0, "… [history boundary]".to_string());
    }
    let no_complete_lines = lines.is_empty();
    if (truncated || bytes_read >= MAX_TAIL_BYTES) && newlines <= count {
        if let Some(first) = lines.first_mut() {
            first.insert_str(0, "… [tail truncated] ");
        } else {
            lines.push("… [tail truncated]".to_string());
        }
    }
    if incomplete_last_line {
        if no_complete_lines
            && lines
                .last()
                .is_some_and(|line| line == "… [tail truncated]")
        {
            lines
                .last_mut()
                .unwrap()
                .push_str(" [incomplete log line omitted]");
        } else {
            lines.push("… [incomplete log line omitted]".to_string());
        }
    }
    Ok(lines)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity {
    first: u64,
    second: u64,
}

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            first: metadata.dev(),
            second: metadata.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or_default();
        FileIdentity {
            first: metadata.len(),
            second: modified,
        }
    }
}

pub struct FileFollower {
    active_path: PathBuf,
    file: File,
    identity: FileIdentity,
    offset: u64,
    partial: Vec<u8>,
    max_partial_line: usize,
    pending: VecDeque<PendingFile>,
    discard_until_newline: bool,
    pending_gap_marker: bool,
}

struct PendingFile {
    path: PathBuf,
    identity: FileIdentity,
}

struct OpenedPendingFile {
    file: File,
    identity: FileIdentity,
}

impl FileFollower {
    pub fn from_end_with_limit(path: &Path, max_partial_line: usize) -> Result<Option<Self>> {
        Self::open(path, true, max_partial_line)
    }

    pub fn from_start_with_limit(path: &Path, max_partial_line: usize) -> Result<Option<Self>> {
        Self::open(path, false, max_partial_line)
    }

    pub fn initial_tail(&mut self, count: usize, rotated_files: usize) -> Result<Vec<String>> {
        let mut sources = Vec::new();
        let mut identities = std::collections::HashSet::new();
        identities.insert(self.identity);
        sources.push(OpenTailSource {
            file: self.file.try_clone()?,
            length: self.offset,
            start_verified: read_boundary_marker(&self.active_path, self.identity),
        });
        for index in 1..=rotated_files {
            let path = rotated_path(&self.active_path, index);
            let Some(pending) = inspect_pending(&path)? else {
                continue;
            };
            if identities.insert(pending.identity) {
                let Some(opened) = open_pending(&pending)? else {
                    continue;
                };
                let length = opened.file.metadata()?.len();
                sources.push(OpenTailSource {
                    file: opened.file,
                    length,
                    start_verified: read_boundary_marker(&path, opened.identity),
                });
            }
        }
        tail_open_sources(&mut sources, count)
    }

    pub fn history_pager(
        &self,
        rotated_files: usize,
        max_line_bytes: usize,
    ) -> Result<HistoryPager> {
        let sources = open_history_sources(
            &self.active_path,
            rotated_files,
            Some((&self.file, self.offset, self.identity)),
        )?;
        HistoryPager::from_sources(sources, max_line_bytes)
    }

    fn open(path: &Path, from_end: bool, max_partial_line: usize) -> Result<Option<Self>> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to follow {}", path.display()));
            }
        };
        let metadata = file.metadata()?;
        let identity = file_identity(&metadata);
        let offset = if from_end {
            file.seek(SeekFrom::End(0))?
        } else {
            0
        };
        let discard_until_newline = if from_end {
            if offset == 0 {
                false
            } else {
                file.seek(SeekFrom::End(-1))?;
                let mut last = [0];
                file.read_exact(&mut last)?;
                file.seek(SeekFrom::Start(offset))?;
                last[0] != b'\n'
            }
        } else {
            !read_boundary_marker(path, identity)
        };
        Ok(Some(Self {
            active_path: path.to_path_buf(),
            file,
            identity,
            offset,
            partial: Vec::new(),
            max_partial_line,
            pending: VecDeque::new(),
            discard_until_newline,
            pending_gap_marker: !from_end && discard_until_newline,
        }))
    }

    pub fn read_new_lines(&mut self) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        if self.pending_gap_marker {
            lines.push("… [log gap after rotation]".to_string());
            self.pending_gap_marker = false;
        }
        lines.extend(self.read_current()?);
        let current_length = self.file.metadata()?.len();
        if self.offset < current_length {
            return Ok(lines);
        }

        if self.pending.is_empty()
            && fs::metadata(&self.active_path)
                .ok()
                .is_some_and(|metadata| file_identity(&metadata) != self.identity)
        {
            match verified_rotation_successors(&self.active_path, self.identity)? {
                Some(successors) => self.pending = successors,
                None => {
                    self.partial.clear();
                    self.discard_until_newline = true;
                    lines.push("… [log gap after rotation]".to_string());
                    if let Some(active) = inspect_pending(&self.active_path)? {
                        self.pending.push_back(active);
                    }
                }
            }
        }

        if let Some(next) = self.pending.pop_front() {
            if let Some(opened) = open_pending(&next)? {
                self.file = opened.file;
                self.identity = opened.identity;
                self.offset = 0;
                lines.extend(self.read_current()?);
            } else {
                self.pending.clear();
                self.partial.clear();
                self.discard_until_newline = true;
                lines.push("… [log gap after rotation]".to_string());
                if let Some(active) = inspect_pending(&self.active_path)? {
                    self.pending.push_back(active);
                }
            }
        }
        Ok(lines)
    }

    fn read_current(&mut self) -> Result<Vec<String>> {
        let length = self.file.metadata()?.len();
        let mut truncated = false;
        if length < self.offset {
            self.file.seek(SeekFrom::Start(0))?;
            self.offset = 0;
            self.partial.clear();
            self.discard_until_newline = true;
            truncated = true;
        } else {
            self.file.seek(SeekFrom::Start(self.offset))?;
        }
        let mut bytes = vec![0; FOLLOW_READ_CHUNK];
        let count = self.file.read(&mut bytes)?;
        bytes.truncate(count);
        self.offset += count as u64;
        self.partial.extend(bytes);

        let mut lines = Vec::new();
        if truncated {
            lines.push("… [log gap after truncation]".to_string());
        }
        if self.discard_until_newline {
            if let Some(newline) = self.partial.iter().position(|byte| *byte == b'\n') {
                self.partial.drain(..=newline);
                self.discard_until_newline = false;
            } else {
                self.partial.clear();
                return Ok(lines);
            }
        }
        while let Some(newline) = self.partial.iter().position(|byte| *byte == b'\n') {
            let line = self.partial.drain(..=newline).collect::<Vec<_>>();
            if line.len().saturating_sub(1) > self.max_partial_line {
                lines.push("… [oversized log line omitted]".to_string());
            } else {
                lines.push(
                    String::from_utf8_lossy(&line)
                        .trim_end_matches(['\r', '\n'])
                        .to_string(),
                );
            }
        }
        if self.partial.len() > self.max_partial_line {
            self.partial.clear();
            self.discard_until_newline = true;
            lines.push("… [oversized log line omitted]".to_string());
        }
        Ok(lines)
    }
}

fn inspect_pending(path: &Path) -> Result<Option<PendingFile>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let identity = file_identity(&file.metadata()?);
    Ok(Some(PendingFile {
        path: path.to_path_buf(),
        identity,
    }))
}

fn open_pending(pending: &PendingFile) -> Result<Option<OpenedPendingFile>> {
    let file = match File::open(&pending.path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let identity = file_identity(&file.metadata()?);
    if identity != pending.identity {
        return Ok(None);
    }
    Ok(Some(OpenedPendingFile { file, identity }))
}

fn verified_rotation_successors(
    active_path: &Path,
    previous: FileIdentity,
) -> Result<Option<VecDeque<PendingFile>>> {
    let Some(parent) = active_path.parent() else {
        return Ok(None);
    };
    let Some(base) = active_path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let mut generations = std::collections::HashMap::new();
    if let Some(active) = inspect_pending(active_path)? {
        generations.insert(0, (active_path.to_path_buf(), active.identity));
    }
    for entry in fs::read_dir(parent)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(index) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix(&format!("{base}.")))
            .and_then(|suffix| suffix.parse::<usize>().ok())
        else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        generations.insert(index, (entry.path(), file_identity(&metadata)));
    }
    let Some(previous_index) = generations
        .iter()
        .find_map(|(index, (_, identity))| (*identity == previous).then_some(*index))
    else {
        return Ok(None);
    };
    if previous_index == 0 {
        return Ok(Some(VecDeque::new()));
    }

    let mut successors = VecDeque::new();
    for index in (0..previous_index).rev() {
        let Some((path, expected)) = generations.get(&index) else {
            return Ok(None);
        };
        successors.push_back(PendingFile {
            path: path.clone(),
            identity: *expected,
        });
    }
    Ok(Some(successors))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "hum-log-{name}-{}-{unique}.log",
            std::process::id()
        ))
    }

    fn policy(max_file_bytes: u64, rotated_files: usize) -> LogPolicy {
        LogPolicy {
            max_file_bytes,
            rotated_files,
            retention: None,
        }
    }

    #[test]
    fn rotation_bounds_bytes_and_file_count() {
        let path = temp_path("rotate");
        let mut writer = RotatingWriter::new(path.clone(), policy(32, 2)).unwrap();
        writer.write_bounded(&[b'x'; 150]).unwrap();
        drop(writer);

        let files = [path.clone(), rotated_path(&path, 1), rotated_path(&path, 2)];
        assert!(files
            .iter()
            .all(|file| file.metadata().unwrap().len() <= 32));
        assert!(!rotated_path(&path, 3).exists());
        let total = files
            .iter()
            .map(|file| file.metadata().unwrap().len())
            .sum::<u64>();
        assert!(total <= 32 * 3);
        for file in files {
            remove_if_exists(&file).unwrap();
        }
    }

    #[test]
    fn existing_oversized_history_is_rebounded_to_the_new_policy() {
        let path = temp_path("rebound");
        let rotated = rotated_path(&path, 1);
        fs::write(&rotated, b"oldest-newest").unwrap();

        cleanup_rotated(&path, policy(6, 1)).unwrap();

        assert_eq!(fs::read(&rotated).unwrap(), b"newest");
        assert_eq!(fs::metadata(&rotated).unwrap().len(), 6);
        remove_if_exists(&rotated).unwrap();
    }

    #[test]
    fn cleanup_removes_orphaned_rebound_temporaries() {
        let path = temp_path("orphan");
        let temporary = PathBuf::from(format!("{}.1.bound.999", path.display()));
        let neighbor = PathBuf::from(format!("{}.bound.worker.stdout.log", path.display()));
        fs::write(&temporary, b"orphan").unwrap();
        fs::write(&neighbor, b"neighbor").unwrap();

        cleanup_rotated(&path, policy(8, 1)).unwrap();

        assert!(!temporary.exists());
        assert_eq!(fs::read(&neighbor).unwrap(), b"neighbor");
        remove_if_exists(&neighbor).unwrap();
    }

    #[test]
    fn zero_retention_removes_rotated_history() {
        let path = temp_path("retention");
        let mut retention = policy(8, 3);
        retention.retention = Some(Duration::ZERO);
        let mut writer = RotatingWriter::new(path.clone(), retention).unwrap();
        writer.write_bounded(b"0123456789abcdef").unwrap();
        drop(writer);
        assert!(!rotated_path(&path, 1).exists());
        remove_if_exists(&path).unwrap();
    }

    #[test]
    fn follower_bounds_memory_for_output_without_newlines_and_survives_rotation() {
        let path = temp_path("follow");
        File::create(&path).unwrap();
        let mut follower = FileFollower::from_end_with_limit(&path, 16)
            .unwrap()
            .unwrap();
        let mut writer = RotatingWriter::new(path.clone(), policy(32, 2)).unwrap();
        writer.write_bounded(&[b'x'; 48]).unwrap();

        let mut emitted = Vec::new();
        for _ in 0..4 {
            emitted.extend(follower.read_new_lines().unwrap());
            assert!(follower.partial.len() <= 16);
        }
        assert!(!emitted.is_empty());
        assert!(emitted
            .iter()
            .all(|line| line == "… [oversized log line omitted]"));
        drop(writer);
        for index in 0..=2 {
            let file = if index == 0 {
                path.clone()
            } else {
                rotated_path(&path, index)
            };
            remove_if_exists(&file).unwrap();
        }
    }

    #[test]
    fn follower_reassembles_a_line_split_by_rotation() {
        let path = temp_path("follow-line-rotation");
        File::create(&path).unwrap();
        let mut follower = FileFollower::from_end_with_limit(&path, 64)
            .unwrap()
            .unwrap();
        let mut writer = RotatingWriter::new(path.clone(), policy(8, 1)).unwrap();
        writer.write_bounded(b"token=secret\n").unwrap();

        assert_eq!(follower.read_new_lines().unwrap(), ["token=secret"]);
        drop(writer);
        remove_if_exists(&path).unwrap();
        remove_if_exists(&rotated_path(&path, 1)).unwrap();
    }

    #[test]
    fn zero_history_rotation_replaces_inode_and_never_emits_a_suffix() {
        let path = temp_path("zero-history-follow");
        let mut writer = RotatingWriter::new(path.clone(), policy(8, 0)).unwrap();
        writer.write_bounded(b"token=se").unwrap();
        let original = file_identity(&fs::metadata(&path).unwrap());
        let mut follower = FileFollower::from_end_with_limit(&path, 64)
            .unwrap()
            .unwrap();

        writer.write_bounded(b"cret\nsafe\n").unwrap();
        let replacement = file_identity(&fs::metadata(&path).unwrap());
        let mut visible = Vec::new();
        for _ in 0..3 {
            visible.extend(follower.read_new_lines().unwrap());
        }

        assert_ne!(original, replacement);
        assert!(visible.iter().all(|line| !line.contains("cret")));
        remove_if_exists(&path).unwrap();
        remove_if_exists(&boundary_path(&path)).unwrap();
    }

    #[test]
    fn reopen_from_start_discards_an_unverified_fragment() {
        let path = temp_path("reopen-boundary");
        fs::write(&path, b"cret\nsafe\n").unwrap();
        let mut follower = FileFollower::from_start_with_limit(&path, 64)
            .unwrap()
            .unwrap();

        let visible = follower.read_new_lines().unwrap();

        assert_eq!(visible, ["… [log gap after rotation]", "safe"]);
        remove_if_exists(&path).unwrap();
    }

    #[test]
    fn boundary_marker_is_bound_to_the_opened_inode() {
        let path = temp_path("marker-identity");
        let mut writer = RotatingWriter::new(path.clone(), policy(8, 0)).unwrap();
        let original = file_identity(&fs::metadata(&path).unwrap());
        writer.write_bounded(b"123456789").unwrap();

        assert!(!read_boundary_marker(&path, original));
        remove_if_exists(&path).unwrap();
        remove_if_exists(&boundary_path(&path)).unwrap();
    }

    #[test]
    fn snapshot_tail_and_follow_have_no_transition_gap() {
        let path = temp_path("snapshot-follow");
        fs::write(&path, b"before\n").unwrap();
        let mut follower = FileFollower::from_end_with_limit(&path, 64)
            .unwrap()
            .unwrap();
        let mut writer = OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(b"during\n").unwrap();
        writer.flush().unwrap();

        let initial = follower.initial_tail(20, 0).unwrap();
        let followed = follower.read_new_lines().unwrap();

        assert!(initial.iter().all(|line| !line.contains("during")));
        assert_eq!(followed, ["during"]);
        remove_if_exists(&path).unwrap();
    }

    #[test]
    fn follower_discards_partial_content_after_falling_behind_retention() {
        let path = temp_path("follow-gap");
        File::create(&path).unwrap();
        let mut follower = FileFollower::from_end_with_limit(&path, 64)
            .unwrap()
            .unwrap();
        let mut writer = RotatingWriter::new(path.clone(), policy(8, 1)).unwrap();
        writer
            .write_bounded(b"token=secret first\ntoken=secret second\ncomplete\n")
            .unwrap();

        let visible = follower.read_new_lines().unwrap();
        assert_eq!(
            visible.first().map(String::as_str),
            Some("… [log gap after rotation]")
        );
        assert!(visible.iter().all(|line| !line.starts_with("cret")));
        drop(writer);
        remove_if_exists(&path).unwrap();
        remove_if_exists(&rotated_path(&path, 1)).unwrap();
    }

    #[test]
    fn tail_reads_rotated_history_and_is_byte_bounded() {
        let path = temp_path("tail");
        let mut writer = RotatingWriter::new(path.clone(), policy(12, 2)).unwrap();
        writer.write_bounded(b"one\ntwo\nthree\nfour\n").unwrap();
        drop(writer);
        assert_eq!(tail_history(&path, 3, 2).unwrap(), ["two", "three", "four"]);

        let mut file = open_log_truncated(&path).unwrap();
        file.write_all(&vec![b'x'; MAX_TAIL_BYTES * 2]).unwrap();
        let tail = tail_history(&path, 2, 0).unwrap();
        assert_eq!(tail.len(), 1);
        assert!(tail[0].starts_with("… [tail truncated] "));
        assert!(tail[0].len() <= MAX_TAIL_BYTES + 32);
        for index in 0..=2 {
            let file = if index == 0 {
                path.clone()
            } else {
                rotated_path(&path, index)
            };
            remove_if_exists(&file).unwrap();
        }
    }

    #[test]
    fn history_pager_walks_backwards_in_bounded_pages() {
        let path = temp_path("history-pager");
        let mut content = (0..300)
            .map(|index| format!("line-{index}\n"))
            .collect::<String>();
        content.push_str("incomplete-secret");
        fs::write(&path, content).unwrap();

        let mut pager = HistoryPager::open(&path, 0, 128).unwrap();
        let newest = pager.next_older(100).unwrap();
        let middle = pager.next_older(100).unwrap();
        let oldest = pager.next_older(100).unwrap();
        let boundary = pager.next_older(100).unwrap();

        assert_eq!(newest.first().map(String::as_str), Some("line-201"));
        assert_eq!(newest.get(98).map(String::as_str), Some("line-299"));
        assert_eq!(
            newest.last().map(String::as_str),
            Some("… [incomplete log line omitted]")
        );
        assert_eq!(middle.first().map(String::as_str), Some("line-101"));
        assert_eq!(middle.last().map(String::as_str), Some("line-200"));
        assert_eq!(oldest.first().map(String::as_str), Some("line-1"));
        assert_eq!(oldest.last().map(String::as_str), Some("line-100"));
        assert_eq!(
            boundary.first().map(String::as_str),
            Some("… [history boundary]")
        );
        assert!(newest
            .iter()
            .chain(&middle)
            .chain(&oldest)
            .chain(&boundary)
            .all(|line| !line.contains("incomplete-secret")));
        assert!(!pager.has_more());
        remove_if_exists(&path).unwrap();
    }

    #[test]
    fn history_pager_omits_incomplete_newest_rotated_fragment_when_active_is_empty() {
        let path = temp_path("history-pager-empty-active");
        fs::write(&path, b"").unwrap();
        fs::write(rotated_path(&path, 1), b"token=unredactable").unwrap();

        let mut pager = HistoryPager::open(&path, 1, 128).unwrap();
        let history = pager.next_older(20).unwrap();

        assert_eq!(history, ["… [incomplete log line omitted]"]);
        assert!(history.iter().all(|line| !line.contains("unredactable")));
        remove_if_exists(&path).unwrap();
        remove_if_exists(&rotated_path(&path, 1)).unwrap();
    }

    #[test]
    fn history_pager_reassembles_lines_split_across_rotations() {
        let path = temp_path("history-pager-split");
        let mut writer = RotatingWriter::new(path.clone(), policy(6, 3)).unwrap();
        writer.write_bounded(b"abcdefgh\nnext\n").unwrap();
        drop(writer);

        let mut pager = HistoryPager::open(&path, 3, 128).unwrap();
        let history = pager.next_older(20).unwrap();

        assert_eq!(history, ["abcdefgh", "next"]);
        for index in 0..=3 {
            let file = if index == 0 {
                path.clone()
            } else {
                rotated_path(&path, index)
            };
            remove_if_exists(&file).unwrap();
            remove_if_exists(&boundary_path(&file)).unwrap();
        }
    }

    #[test]
    fn tail_omits_an_incomplete_last_line() {
        let path = temp_path("incomplete-tail");
        fs::write(&path, b"safe\ntoken=ABC").unwrap();

        let tail = tail_history(&path, 20, 0).unwrap();

        assert!(tail.iter().all(|line| !line.contains("token=ABC")));
        assert_eq!(
            tail.last().map(String::as_str),
            Some("… [incomplete log line omitted]")
        );
        remove_if_exists(&path).unwrap();
    }

    #[test]
    fn retained_history_never_exposes_an_unredactable_partial_first_line() {
        let path = temp_path("history-boundary");
        let mut writer = RotatingWriter::new(path.clone(), policy(8, 1)).unwrap();
        writer
            .write_bounded(b"token=secret first\ntoken=secret second\n")
            .unwrap();
        drop(writer);

        let history = tail_history(&path, 100, 1).unwrap();
        assert_eq!(
            history.first().map(String::as_str),
            Some("… [history boundary]")
        );
        assert!(history.iter().all(|line| !line.starts_with("cret")));
        for index in 0..=1 {
            let file = if index == 0 {
                path.clone()
            } else {
                rotated_path(&path, index)
            };
            remove_if_exists(&file).unwrap();
        }
    }

    #[test]
    fn active_only_history_never_exposes_an_unverified_first_fragment() {
        let path = temp_path("active-boundary");
        fs::write(&path, b"cret\nsafe\n").unwrap();

        let history = tail_history(&path, 20, 0).unwrap();

        assert_eq!(history, ["… [history boundary]", "safe"]);
        assert!(history.iter().all(|line| !line.contains("cret")));
        remove_if_exists(&path).unwrap();
    }

    #[test]
    fn redactor_masks_every_configured_pattern() {
        let redactor = Redactor::new(&["token=[^ ]+".to_string(), "secret".to_string()]).unwrap();
        assert_eq!(
            redactor.redact("token=abc secret visible"),
            "[REDACTED] [REDACTED] visible"
        );
        assert_eq!(
            redactor.redact_bounded("token=secret", 4),
            "… [oversized log line omitted]"
        );
    }
}
