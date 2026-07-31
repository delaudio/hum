use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use super::error::ConfigError;
use super::model::Config;
use super::validate;

pub const CONFIG_FILE: &str = "hum.yaml";
pub const LOCAL_CONFIG_FILE: &str = "hum.local.yaml";

/// Result of resolving the configuration: the merged, validated config plus
/// the paths that contributed to it (useful for error messages and `doctor`).
pub struct Loaded {
    pub config: Config,
    pub base_path: PathBuf,
    pub local_path: Option<PathBuf>,
    /// Directory the config was found in — services/repositories with
    /// relative paths are resolved against this.
    pub root_dir: PathBuf,
}

/// RF-02: discover `hum.yaml` by:
/// 1. an explicit `--config` path
/// 2. the current directory
/// 3. walking up parent directories
/// 4. an optional global path (`$XDG_CONFIG_HOME/hum/hum.yaml` or `~/.config/hum/hum.yaml`)
pub fn discover(explicit: Option<&Path>) -> Result<PathBuf, ConfigError> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(path.to_path_buf())
        } else {
            Err(ConfigError::Io {
                file: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "explicit --config path does not exist",
                ),
            })
        };
    }

    let cwd = env::current_dir().map_err(|source| ConfigError::Io {
        file: PathBuf::from("."),
        source,
    })?;

    let mut dir = Some(cwd.as_path());
    while let Some(d) = dir {
        let candidate = d.join(CONFIG_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
        dir = d.parent();
    }

    if let Some(config_home) = dirs::config_dir() {
        let candidate = config_home.join("hum").join(CONFIG_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(ConfigError::NotFound)
}

/// RF-01 + RF-02 + local override (section 10.3/10.4): load, merge and validate.
pub fn load(explicit: Option<&Path>) -> Result<Loaded, ConfigError> {
    let base_path = discover(explicit)?;
    let root_dir = base_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let base_value = read_yaml(&base_path)?;

    let local_path = root_dir.join(LOCAL_CONFIG_FILE);
    let (merged_value, local_path_opt) = if local_path.is_file() {
        let local_value = read_yaml(&local_path)?;
        (deep_merge(base_value, local_value), Some(local_path))
    } else {
        (base_value, None)
    };

    let config: Config = serde_yaml::from_value(merged_value)
        .map_err(|e| ConfigError::from_yaml(base_path.clone(), e))?;

    validate::validate(&config, &base_path)?;

    Ok(Loaded {
        config,
        base_path,
        local_path: local_path_opt,
        root_dir,
    })
}

fn read_yaml(path: &Path) -> Result<serde_yaml::Value, ConfigError> {
    let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        file: path.to_path_buf(),
        source,
    })?;
    serde_yaml::from_str(&contents).map_err(|e| ConfigError::from_yaml(path.to_path_buf(), e))
}

/// Deep-merge two YAML values: mappings merge key by key (override wins on
/// conflicting scalars/sequences), everything else is replaced outright.
fn deep_merge(base: serde_yaml::Value, over: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    match (base, over) {
        (Value::Mapping(mut base_map), Value::Mapping(over_map)) => {
            for (k, v) in over_map {
                let merged = match base_map.remove(&k) {
                    Some(base_v) => deep_merge(base_v, v),
                    None => v,
                };
                base_map.insert(k, merged);
            }
            Value::Mapping(base_map)
        }
        (_, over) => over,
    }
}

/// Expand a leading `~` to the user's home directory.
pub fn expand_home(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}
