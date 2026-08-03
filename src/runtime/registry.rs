use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const ENTRY_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeEntry {
    pub version: u32,
    pub project: String,
    pub service: String,
    pub pid: u32,
    pub pgid: i32,
    #[serde(default)]
    pub log_sink_pid: Option<u32>,
    #[serde(default)]
    pub log_sink_start_time: Option<u64>,
    pub process_start_time: u64,
    pub runtime_token: String,
    pub identity_file: PathBuf,
    pub command_hash: String,
    pub config_hash: String,
    pub port: Option<u16>,
    pub cwd: PathBuf,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
    pub started_at: DateTime<Utc>,
}

impl RuntimeEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project: String,
        service: String,
        pid: u32,
        pgid: i32,
        log_sink_pid: Option<u32>,
        log_sink_start_time: Option<u64>,
        process_start_time: u64,
        runtime_token: String,
        identity_file: PathBuf,
        command_hash: String,
        config_hash: String,
        port: Option<u16>,
        cwd: PathBuf,
        stdout_log: PathBuf,
        stderr_log: PathBuf,
    ) -> Self {
        Self {
            version: ENTRY_VERSION,
            project,
            service,
            pid,
            pgid,
            log_sink_pid,
            log_sink_start_time,
            process_start_time,
            runtime_token,
            identity_file,
            command_hash,
            config_hash,
            port,
            cwd,
            stdout_log,
            stderr_log,
            started_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityStatus {
    Matching,
    Missing,
    Mismatch(String),
}

#[derive(Debug, Clone)]
pub struct RuntimeRegistry {
    project: String,
    root: PathBuf,
    runtime_dir: PathBuf,
    logs_dir: PathBuf,
    identity_dir: PathBuf,
    lock_path: PathBuf,
}

impl RuntimeRegistry {
    pub fn for_project(project: &str) -> Result<Self> {
        Self::at(state_home(), project)
    }

    pub fn at(state_root: PathBuf, project: &str) -> Result<Self> {
        if !crate::config::validate::is_safe_identifier(project) {
            anyhow::bail!("unsafe project identifier '{project}'");
        }
        let root = state_root.join("hum").join(project);
        let runtime_dir = root.join("runtime");
        let logs_dir = root.join("logs");
        let identity_dir = root.join("identity");
        fs::create_dir_all(&runtime_dir)
            .with_context(|| format!("failed to create {}", runtime_dir.display()))?;
        fs::create_dir_all(&logs_dir)
            .with_context(|| format!("failed to create {}", logs_dir.display()))?;
        fs::create_dir_all(&identity_dir)
            .with_context(|| format!("failed to create {}", identity_dir.display()))?;
        restrict_directory(&root)?;
        restrict_directory(&runtime_dir)?;
        restrict_directory(&logs_dir)?;
        restrict_directory(&identity_dir)?;
        Ok(Self {
            project: project.to_string(),
            lock_path: root.join("project.lock"),
            root,
            runtime_dir,
            logs_dir,
            identity_dir,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn log_paths(&self, service: &str) -> (PathBuf, PathBuf) {
        (
            self.logs_dir.join(format!("{service}.stdout.log")),
            self.logs_dir.join(format!("{service}.stderr.log")),
        )
    }

    pub fn prepare_exit_code(&self, service: &str) -> Result<PathBuf> {
        if !crate::config::validate::is_safe_identifier(service) {
            anyhow::bail!("unsafe service identifier '{service}'");
        }
        let path = self.exit_code_path(service);
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to prepare {}", path.display()))?;
        restrict_file(&file)?;
        Ok(path)
    }

    pub fn read_exit_code(&self, service: &str) -> Result<Option<i32>> {
        if !crate::config::validate::is_safe_identifier(service) {
            anyhow::bail!("unsafe service identifier '{service}'");
        }
        let path = self.exit_code_path(service);
        let value = match fs::read_to_string(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(value.trim().parse::<i32>().ok())
    }

    pub fn lock(&self) -> Result<ProjectLock> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .with_context(|| format!("failed to open lock {}", self.lock_path.display()))?;
        restrict_file(&file)?;
        lock_exclusive(&file)
            .with_context(|| format!("failed to lock {}", self.lock_path.display()))?;
        Ok(ProjectLock { file })
    }

    pub fn load(&self, service: &str) -> Result<Option<RuntimeEntry>> {
        if !crate::config::validate::is_safe_identifier(service) {
            anyhow::bail!("unsafe service identifier '{service}'");
        }
        let path = self.entry_path(service);
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read runtime entry {}", path.display()))?;
        let entry: RuntimeEntry = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse runtime entry {}", path.display()))?;
        if entry.version != ENTRY_VERSION
            || entry.project != self.project
            || entry.service != service
            || !self.valid_identity_path(&entry)
        {
            anyhow::bail!(
                "runtime entry {} identity does not match {}/{}",
                path.display(),
                self.project,
                service
            );
        }
        Ok(Some(entry))
    }

    pub fn write(&self, entry: &RuntimeEntry) -> Result<()> {
        if entry.version != ENTRY_VERSION
            || entry.project != self.project
            || !crate::config::validate::is_safe_identifier(&entry.service)
        {
            anyhow::bail!("refusing to write mismatched or unsafe runtime entry");
        }
        let path = self.entry_path(&entry.service);
        let temp = self
            .runtime_dir
            .join(format!(".{}.{}.tmp", entry.service, std::process::id()));
        let bytes = serde_json::to_vec_pretty(entry)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)
            .with_context(|| format!("failed to create {}", temp.display()))?;
        restrict_file(&file)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, &path).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                path.display(),
                temp.display()
            )
        })?;
        sync_directory(&self.runtime_dir)?;
        Ok(())
    }

    pub fn remove(&self, service: &str) -> Result<()> {
        if !crate::config::validate::is_safe_identifier(service) {
            anyhow::bail!("unsafe service identifier '{service}'");
        }
        let path = self.entry_path(service);
        let identity_file = self
            .load(service)?
            .map(|entry| entry.identity_file)
            .filter(|path| path.starts_with(&self.identity_dir));
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove stale entry {}", path.display()))?;
            sync_directory(&self.runtime_dir)?;
        }
        if let Some(identity_file) = identity_file {
            if identity_file.exists() {
                fs::remove_file(&identity_file).with_context(|| {
                    format!("failed to remove identity file {}", identity_file.display())
                })?;
                sync_directory(&self.identity_dir)?;
            }
        }
        Ok(())
    }

    pub fn create_identity(&self, service: &str, token: &str) -> Result<IdentityLease> {
        if !crate::config::validate::is_safe_identifier(service)
            || token.len() != 64
            || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("invalid runtime identity for service '{service}'");
        }
        let path = self.identity_dir.join(format!("{service}.{token}.lock"));
        let file = secure_open_new(&path)
            .with_context(|| format!("failed to create identity file {}", path.display()))?;
        restrict_file(&file)?;
        lock_exclusive(&file)?;
        Ok(IdentityLease { file, path })
    }

    fn entry_path(&self, service: &str) -> PathBuf {
        self.runtime_dir.join(format!("{service}.json"))
    }

    fn exit_code_path(&self, service: &str) -> PathBuf {
        self.runtime_dir.join(format!("{service}.exit"))
    }

    fn valid_identity_path(&self, entry: &RuntimeEntry) -> bool {
        entry.identity_file.parent() == Some(self.identity_dir.as_path())
            && entry.identity_file.file_name().is_some_and(|name| {
                name.to_string_lossy() == format!("{}.{}.lock", entry.service, entry.runtime_token)
            })
    }
}

pub struct IdentityLease {
    file: File,
    path: PathBuf,
}

impl IdentityLease {
    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub struct ProjectLock {
    file: File,
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        unlock(&self.file);
    }
}

pub fn inspect_identity(entry: &RuntimeEntry) -> IdentityStatus {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    let pid = sysinfo::Pid::from_u32(entry.pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::new(),
    );

    let leader_alive = system.process(pid).is_some_and(|leader| {
        !matches!(
            leader.status(),
            sysinfo::ProcessStatus::Dead | sysinfo::ProcessStatus::Zombie
        )
    });
    if let Some(leader) = system.process(pid) {
        if leader_alive {
            #[cfg(unix)]
            let current_pgid = unsafe { libc::getpgid(entry.pid as i32) };
            #[cfg(not(unix))]
            let current_pgid = entry.pid as i32;
            if current_pgid != entry.pgid || leader.start_time() != entry.process_start_time {
                return IdentityStatus::Mismatch(format!(
                    "PID {} start time or process group no longer matches its registry entry",
                    entry.pid
                ));
            }
        }
    }

    match identity_lock_is_held(&entry.identity_file) {
        // The group can disappear from a process snapshot just before its
        // final inherited descriptor closes. Keep polling the lock instead of
        // deleting the registry during that exit window.
        Ok(true) => return IdentityStatus::Matching,
        Ok(false) => {}
        Err(error) => {
            return IdentityStatus::Mismatch(format!(
                "could not verify runtime identity lock: {error}"
            ));
        }
    }

    if leader_alive {
        IdentityStatus::Mismatch(format!(
            "process group {} no longer holds its runtime identity lock",
            entry.pgid
        ))
    } else {
        IdentityStatus::Missing
    }
}

fn identity_lock_is_held(path: &Path) -> Result<bool> {
    let file = secure_open_existing(path)?;
    match try_lock_exclusive(&file) {
        Ok(()) => {
            unlock(&file);
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error.into()),
    }
}

pub fn new_runtime_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("failed to generate runtime identity token: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn process_start_time(pid: u32) -> Result<u64> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    for _ in 0..20 {
        let process_pid = sysinfo::Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[process_pid]),
            true,
            ProcessRefreshKind::new(),
        );
        if let Some(process) = system.process(process_pid) {
            return Ok(process.start_time());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    anyhow::bail!("could not inspect newly started PID {pid}")
}

fn state_home() -> PathBuf {
    if let Ok(path) = std::env::var("XDG_STATE_HOME") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("state")
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file(_file: &File) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_open_new(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn secure_open_new(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
}

#[cfg(unix)]
fn secure_open_existing(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn secure_open_existing(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

#[cfg(unix)]
fn unlock(file: &File) {
    use std::os::fd::AsRawFd;
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

#[cfg(not(unix))]
fn unlock(_file: &File) {}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("hum-{name}-{}-{unique}", std::process::id()))
    }

    fn entry(service: &str, token: String, identity_file: PathBuf) -> RuntimeEntry {
        RuntimeEntry::new(
            "demo".to_string(),
            service.to_string(),
            std::process::id(),
            std::process::id() as i32,
            None,
            None,
            1,
            token,
            identity_file,
            "command".to_string(),
            "config".to_string(),
            Some(3000),
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/stdout"),
            PathBuf::from("/tmp/stderr"),
        )
    }

    #[test]
    fn atomically_round_trips_runtime_entry() {
        let state = temp_root("registry");
        let registry = RuntimeRegistry::at(state.clone(), "demo").unwrap();
        let _lock = registry.lock().unwrap();
        let token = "a".repeat(64);
        let identity = registry.create_identity("api", &token).unwrap();
        let expected = entry("api", token, identity.path().to_path_buf());
        registry.write(&expected).unwrap();
        let actual = registry.load("api").unwrap().unwrap();
        assert_eq!(actual.pid, expected.pid);
        assert_eq!(actual.command_hash, expected.command_hash);
        registry.remove("api").unwrap();
        assert!(registry.load("api").unwrap().is_none());
        fs::remove_dir_all(state).unwrap();
    }
}
