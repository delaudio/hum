use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::loader::{expand_home, Loaded};

pub const REGISTRY_FILE: &str = "config.yaml";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, ProjectRegistration>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectRegistration {
    config: PathBuf,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("hum project registry not found at {0}\n  → create it or pass --registry/--config")]
    NotFound(PathBuf),

    #[error("failed to read project registry {file}: {source}")]
    Io {
        file: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid project registry {file}: {description}")]
    Parse { file: PathBuf, description: String },

    #[error("{file}: version: unsupported project registry version {actual}; expected version 1")]
    UnsupportedVersion { file: PathBuf, actual: u32 },

    #[error("unknown project '{0}'\n  → add it under `projects:` in the hum registry")]
    UnknownProject(String),

    #[error("project name '{0}' is reserved by the hum CLI")]
    ReservedProject(String),

    #[error("project name '{0}' contains unsafe path characters")]
    InvalidProject(String),

    #[error("project configuration identifies itself as '{actual}', not '{requested}'")]
    ProjectMismatch { requested: String, actual: String },

    #[error("failed to resolve project configuration {file}: {source}")]
    ConfigPath {
        file: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write project registry {file}: {source}")]
    Write {
        file: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to serialize project registry {file}: {description}")]
    Serialize { file: PathBuf, description: String },

    #[error(transparent)]
    Config(#[from] super::error::ConfigError),
}

struct RegistryLock(fs::File);

impl RegistryLock {
    fn acquire(registry_path: &Path) -> Result<Self, RegistryError> {
        let parent = registry_path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| RegistryError::Write {
            file: registry_path.to_path_buf(),
            source,
        })?;
        let lock_path = parent.join(format!(
            ".{}.lock",
            registry_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("config")
        ));
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&lock_path)
            .map_err(|source| RegistryError::Write {
                file: registry_path.to_path_buf(),
                source,
            })?;
        file.lock().map_err(|source| RegistryError::Write {
            file: registry_path.to_path_buf(),
            source,
        })?;
        Ok(Self(file))
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Resolve a selected project either from an explicit project config or from
/// the global registry. Explicit config is useful during migration and tests.
pub fn resolve_project(
    project: &str,
    explicit_config: Option<&Path>,
    explicit_registry: Option<&Path>,
) -> Result<Loaded, RegistryError> {
    if is_reserved_project_name(project) {
        return Err(RegistryError::ReservedProject(project.to_string()));
    }
    if !super::validate::is_safe_identifier(project) {
        return Err(RegistryError::InvalidProject(project.to_string()));
    }
    let loaded = if let Some(config) = explicit_config {
        super::loader::load(Some(config))?
    } else {
        let registry_path = explicit_registry
            .map(Path::to_path_buf)
            .unwrap_or_else(default_registry_path);
        let registry = read_registry(&registry_path)?;
        let registration = registry
            .projects
            .get(project)
            .ok_or_else(|| RegistryError::UnknownProject(project.to_string()))?;
        let expanded = expand_home(&registration.config);
        let project_path = if expanded.is_absolute() {
            expanded
        } else {
            registry_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(expanded)
        };
        super::loader::load(Some(&project_path))?
    };

    if let Some(actual) = &loaded.config.project {
        if actual != project {
            return Err(RegistryError::ProjectMismatch {
                requested: project.to_string(),
                actual: actual.clone(),
            });
        }
    }

    Ok(loaded)
}

/// Validate and register a project configuration in the machine-local registry.
/// The canonical path is intentionally machine-specific; project files remain
/// portable because their own relative references resolve from `hum.yaml`.
pub fn register_project(
    project: &str,
    config: &Path,
    explicit_registry: Option<&Path>,
) -> Result<(PathBuf, PathBuf), RegistryError> {
    validate_project_name(project)?;

    let loaded = super::loader::load(Some(config))?;
    if let Some(actual) = &loaded.config.project {
        if actual != project {
            return Err(RegistryError::ProjectMismatch {
                requested: project.to_string(),
                actual: actual.clone(),
            });
        }
    }
    let config_path =
        fs::canonicalize(&loaded.base_path).map_err(|source| RegistryError::ConfigPath {
            file: loaded.base_path.clone(),
            source,
        })?;
    let registry_path = explicit_registry
        .map(Path::to_path_buf)
        .unwrap_or_else(default_registry_path);
    let _lock = RegistryLock::acquire(&registry_path)?;
    let mut registry = if registry_path.is_file() {
        read_registry(&registry_path)?
    } else {
        Registry {
            version: 1,
            projects: BTreeMap::new(),
        }
    };
    registry.projects.insert(
        project.to_string(),
        ProjectRegistration {
            config: config_path.clone(),
        },
    );
    write_registry(&registry_path, &registry)?;
    Ok((registry_path, config_path))
}

fn validate_project_name(project: &str) -> Result<(), RegistryError> {
    if is_reserved_project_name(project) {
        return Err(RegistryError::ReservedProject(project.to_string()));
    }
    if !super::validate::is_safe_identifier(project) {
        return Err(RegistryError::InvalidProject(project.to_string()));
    }
    Ok(())
}

pub fn is_reserved_project_name(name: &str) -> bool {
    matches!(
        name,
        "start"
            | "stop"
            | "restart"
            | "reset"
            | "status"
            | "plan"
            | "secrets"
            | "logs"
            | "doctor"
            | "tui"
            | "config"
            | "project"
            | "help"
            | "up"
            | "down"
    )
}

pub fn default_registry_path() -> PathBuf {
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("hum").join(REGISTRY_FILE);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("hum")
        .join(REGISTRY_FILE)
}

fn read_registry(path: &Path) -> Result<Registry, RegistryError> {
    if !path.is_file() {
        return Err(RegistryError::NotFound(path.to_path_buf()));
    }
    let contents = fs::read_to_string(path).map_err(|source| RegistryError::Io {
        file: path.to_path_buf(),
        source,
    })?;
    let registry: Registry =
        yaml_serde::from_str(&contents).map_err(|error| RegistryError::Parse {
            file: path.to_path_buf(),
            description: error.to_string(),
        })?;
    if registry.version != 1 {
        return Err(RegistryError::UnsupportedVersion {
            file: path.to_path_buf(),
            actual: registry.version,
        });
    }
    if let Some(name) = registry
        .projects
        .keys()
        .find(|name| is_reserved_project_name(name))
    {
        return Err(RegistryError::ReservedProject(name.clone()));
    }
    if let Some(name) = registry
        .projects
        .keys()
        .find(|name| !super::validate::is_safe_identifier(name))
    {
        return Err(RegistryError::InvalidProject(name.clone()));
    }
    Ok(registry)
}

fn write_registry(path: &Path, registry: &Registry) -> Result<(), RegistryError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| RegistryError::Write {
        file: path.to_path_buf(),
        source,
    })?;
    let contents = yaml_serde::to_string(registry).map_err(|error| RegistryError::Serialize {
        file: path.to_path_buf(),
        description: error.to_string(),
    })?;
    let temp_path = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    let result = (|| -> std::io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(RegistryError::Write {
            file: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("hum-{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn resolves_relative_project_config_from_registry() {
        let root = test_dir("registry");
        let project = root.join("demo.yaml");
        fs::write(
            &project,
            "version: 2\nproject: demo\nservices: {}\ntemplates:\n  all-services:\n    services: []\n",
        )
        .unwrap();
        let registry = root.join("config.yaml");
        fs::write(
            &registry,
            "version: 1\nprojects:\n  demo:\n    config: demo.yaml\n",
        )
        .unwrap();

        let loaded = resolve_project("demo", None, Some(&registry)).unwrap();
        assert_eq!(loaded.config.project.as_deref(), Some("demo"));
        assert!(loaded.config.templates.contains_key("all-services"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_unknown_project() {
        let root = test_dir("unknown-project");
        let registry = root.join("config.yaml");
        fs::write(&registry, "version: 1\nprojects: {}\n").unwrap();

        let error = match resolve_project("missing", None, Some(&registry)) {
            Ok(_) => panic!("unknown project unexpectedly resolved"),
            Err(error) => error,
        };
        assert!(matches!(error, RegistryError::UnknownProject(name) if name == "missing"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_project_names_reserved_by_cli_commands() {
        let error = match resolve_project("status", None, None) {
            Ok(_) => panic!("reserved project unexpectedly resolved"),
            Err(error) => error,
        };
        assert!(matches!(error, RegistryError::ReservedProject(name) if name == "status"));
    }

    #[test]
    fn registers_project_with_canonical_config_path() {
        let root = test_dir("register-project");
        let project = root.join("demo.yaml");
        fs::write(
            &project,
            "version: 2\nproject: demo\nservices: {}\ntemplates:\n  all-services:\n    services: []\n",
        )
        .unwrap();
        let registry = root.join("registry").join("config.yaml");

        let (written_registry, written_config) =
            register_project("demo", &project, Some(&registry)).unwrap();

        assert_eq!(written_registry, registry);
        assert_eq!(written_config, fs::canonicalize(&project).unwrap());
        let loaded = resolve_project("demo", None, Some(&written_registry)).unwrap();
        assert_eq!(loaded.config.project.as_deref(), Some("demo"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registration_preserves_existing_projects_and_replaces_named_entry() {
        let root = test_dir("register-preserve");
        let first = root.join("first.yaml");
        let replacement = root.join("nested").join("first.yaml");
        let second = root.join("second.yaml");
        fs::create_dir_all(replacement.parent().unwrap()).unwrap();
        for (path, name) in [
            (&first, "first"),
            (&replacement, "first"),
            (&second, "second"),
        ] {
            fs::write(
                path,
                format!(
                    "version: 2\nproject: {name}\nservices: {{}}\ntemplates:\n  all-services:\n    services: []\n"
                ),
            )
            .unwrap();
        }
        let registry = root.join("config.yaml");

        register_project("first", &first, Some(&registry)).unwrap();
        register_project("second", &second, Some(&registry)).unwrap();
        register_project("first", &replacement, Some(&registry)).unwrap();

        let contents = fs::read_to_string(&registry).unwrap();
        assert!(contents.contains("first:"));
        assert!(contents.contains("second:"));
        let loaded = resolve_project("first", None, Some(&registry)).unwrap();
        assert_eq!(loaded.base_path, fs::canonicalize(replacement).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_registrations_do_not_lose_projects() {
        let root = test_dir("register-concurrent");
        let registry = root.join("config.yaml");
        let mut workers = Vec::new();
        for index in 0..8 {
            let name = format!("project-{index}");
            let project = root.join(format!("{name}.yaml"));
            fs::write(
                &project,
                format!(
                    "version: 2\nproject: {name}\nservices: {{}}\ntemplates:\n  all-services:\n    services: []\n"
                ),
            )
            .unwrap();
            let registry = registry.clone();
            workers.push(std::thread::spawn(move || {
                register_project(&name, &project, Some(&registry)).unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        for index in 0..8 {
            let name = format!("project-{index}");
            resolve_project(&name, None, Some(&registry)).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
}
