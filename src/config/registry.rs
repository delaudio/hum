use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use super::loader::{expand_home, Loaded};

pub const REGISTRY_FILE: &str = "config.yaml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    version: u32,
    #[serde(default)]
    projects: HashMap<String, ProjectRegistration>,
}

#[derive(Debug, Deserialize)]
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

    #[error(transparent)]
    Config(#[from] super::error::ConfigError),
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
}
