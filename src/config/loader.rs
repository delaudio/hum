use std::collections::HashSet;
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
#[derive(Debug)]
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

    if let Some(candidate) = global_config_path() {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(ConfigError::NotFound)
}

/// `$XDG_CONFIG_HOME/hum/hum.yaml`, falling back to `~/.config/hum/hum.yaml`.
/// Deliberately XDG-style on every platform (not `dirs::config_dir()`,
/// which resolves to `~/Library/Application Support` on macOS) since
/// `~/.config/hum` is where the docs tell users to put this file.
pub fn global_config_path() -> Option<PathBuf> {
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("hum").join(CONFIG_FILE));
        }
    }
    dirs::home_dir().map(|home| home.join(".config").join("hum").join(CONFIG_FILE))
}

/// RF-01 + RF-02 + local override (section 10.3/10.4): load, merge and validate.
pub fn load(explicit: Option<&Path>) -> Result<Loaded, ConfigError> {
    let base_path = discover(explicit)?;
    let root_dir = base_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let (base_value, base_source) = read_yaml(&base_path)?;
    yaml_serde::from_value::<Config>(base_value.clone()).map_err(|error| {
        ConfigError::from_yaml_with_source(base_path.clone(), error, &base_source)
    })?;

    let local_path = root_dir.join(LOCAL_CONFIG_FILE);
    let (merged_value, local_path_opt, local_source, local_fields) = if local_path.is_file() {
        let (local_value, local_source) = read_yaml(&local_path)?;
        validate_partial_schema(&local_value, &local_path, &local_source)?;
        let local_fields = collect_leaf_paths(&local_value);
        (
            deep_merge(base_value, local_value),
            Some(local_path),
            Some(local_source),
            local_fields,
        )
    } else {
        (base_value, None, None, HashSet::new())
    };

    let config: Config = yaml_serde::from_value(merged_value).map_err(|error| {
        if let (Some(local_path), Some(local_source)) = (&local_path_opt, &local_source) {
            ConfigError::from_yaml_with_source(local_path.clone(), error, local_source)
        } else {
            ConfigError::from_yaml_with_source(base_path.clone(), error, &base_source)
        }
    })?;

    if let Err(mut error) = validate::validate(&config, &base_path) {
        let changed_by_local = match &error {
            ConfigError::Validation {
                field, description, ..
            } => validation_touches_local_field(field, description, &local_fields),
            _ => false,
        };
        if changed_by_local {
            if let (Some(local_path), ConfigError::Validation { file, .. }) =
                (&local_path_opt, &mut error)
            {
                *file = local_path.clone();
            }
        }
        return Err(error);
    }

    Ok(Loaded {
        config,
        base_path,
        local_path: local_path_opt,
        root_dir,
    })
}

fn collect_leaf_paths(value: &yaml_serde::Value) -> HashSet<String> {
    fn visit(value: &yaml_serde::Value, prefix: &str, paths: &mut HashSet<String>) {
        if let Some(mapping) = value.as_mapping() {
            if mapping.is_empty() && !prefix.is_empty() {
                paths.insert(prefix.to_string());
            }
            for (key, value) in mapping {
                if let Some(key) = key.as_str() {
                    let path = if prefix.is_empty() {
                        key.to_string()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    visit(value, &path, paths);
                }
            }
        } else if !prefix.is_empty() {
            paths.insert(prefix.to_string());
        }
    }

    let mut paths = HashSet::new();
    visit(value, "", &mut paths);
    paths
}

fn validation_touches_local_field(
    field: &str,
    description: &str,
    local_fields: &HashSet<String>,
) -> bool {
    let matches = |candidate: &str| {
        local_fields.iter().any(|local| {
            field_path_matches(candidate, local)
                || local.starts_with(&format!("{candidate}."))
                || candidate.starts_with(&format!("{local}."))
        })
    };
    if matches(field) {
        return true;
    }

    // Port collision diagnostics name the other service; attribute the error
    // to local when either side of the collision was overridden there.
    description
        .split_once("also assigned to service '")
        .and_then(|(_, rest)| rest.split_once('\''))
        .is_some_and(|(service, _)| matches(&format!("services.{service}.port")))
}

fn field_path_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.split('.').collect::<Vec<_>>();
    let path = path.split('.').collect::<Vec<_>>();
    pattern.len() == path.len()
        && pattern
            .iter()
            .zip(path)
            .all(|(expected, actual)| *expected == "*" || *expected == actual)
}

fn read_yaml(path: &Path) -> Result<(yaml_serde::Value, String), ConfigError> {
    let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        file: path.to_path_buf(),
        source,
    })?;
    let value = yaml_serde::from_str(&contents)
        .map_err(|e| ConfigError::from_yaml(path.to_path_buf(), e))?;
    Ok((value, contents))
}

fn validate_partial_schema(
    value: &yaml_serde::Value,
    file: &Path,
    source: &str,
) -> Result<(), ConfigError> {
    const CONFIG: &[&str] = &[
        "version",
        "project",
        "repositories",
        "services",
        "templates",
        "profiles",
        "logs",
    ];
    const REPOSITORY: &[&str] = &["path"];
    const SERVICE: &[&str] = &[
        "repository",
        "cwd",
        "command",
        "port",
        "url",
        "env_file",
        "env",
        "depends_on",
        "healthcheck",
        "requires",
        "depends_on_ready",
    ];
    const REQUIREMENTS: &[&str] = &["commands", "files", "env"];
    const HEALTHCHECK: &[&str] = &[
        "type",
        "url",
        "host",
        "port",
        "timeout",
        "interval",
        "retries",
        "expected_status",
    ];
    const TEMPLATE: &[&str] = &["services"];
    const LOGS: &[&str] = &[
        "max_file_bytes",
        "rotated_files",
        "max_line_bytes",
        "retention",
        "redact_patterns",
    ];

    let root = mapping(value, file)?;
    check_keys(root, CONFIG, file, source)?;

    if let Some(repositories) = root
        .get("repositories")
        .and_then(yaml_serde::Value::as_mapping)
    {
        for repository in repositories
            .values()
            .filter_map(yaml_serde::Value::as_mapping)
        {
            check_keys(repository, REPOSITORY, file, source)?;
        }
    }
    if let Some(services) = root.get("services").and_then(yaml_serde::Value::as_mapping) {
        for service in services.values().filter_map(yaml_serde::Value::as_mapping) {
            check_keys(service, SERVICE, file, source)?;
            if let Some(requires) = service
                .get("requires")
                .and_then(yaml_serde::Value::as_mapping)
            {
                check_keys(requires, REQUIREMENTS, file, source)?;
            }
            if let Some(healthcheck) = service
                .get("healthcheck")
                .and_then(yaml_serde::Value::as_mapping)
            {
                check_keys(healthcheck, HEALTHCHECK, file, source)?;
            }
        }
    }
    for templates_key in ["templates", "profiles"] {
        if let Some(templates) = root
            .get(templates_key)
            .and_then(yaml_serde::Value::as_mapping)
        {
            for template in templates.values().filter_map(yaml_serde::Value::as_mapping) {
                check_keys(template, TEMPLATE, file, source)?;
            }
        }
    }
    if let Some(logs) = root.get("logs").and_then(yaml_serde::Value::as_mapping) {
        check_keys(logs, LOGS, file, source)?;
    }
    Ok(())
}

fn mapping<'a>(
    value: &'a yaml_serde::Value,
    file: &Path,
) -> Result<&'a yaml_serde::Mapping, ConfigError> {
    value.as_mapping().ok_or_else(|| {
        ConfigError::validation(
            file,
            "root",
            "configuration root must be a YAML mapping",
            "use key/value sections such as `services:` and `templates:`",
        )
    })
}

fn check_keys(
    mapping: &yaml_serde::Mapping,
    allowed: &[&str],
    file: &Path,
    source: &str,
) -> Result<(), ConfigError> {
    for key in mapping.keys().filter_map(yaml_serde::Value::as_str) {
        if !allowed.contains(&key) {
            return Err(ConfigError::unknown_field(file, key, allowed, source));
        }
    }
    Ok(())
}

/// Deep-merge two YAML values: mappings merge key by key (override wins on
/// conflicting scalars/sequences), everything else is replaced outright.
fn deep_merge(base: yaml_serde::Value, over: yaml_serde::Value) -> yaml_serde::Value {
    use yaml_serde::Value;
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn rejects_unknown_config_fields() {
        let yaml = r#"
version: 2
project: demo
services:
  api:
    command: cargo run
    comand: typo
templates:
  all:
    services: [api]
"#;
        let error = yaml_serde::from_str::<Config>(yaml).unwrap_err();
        assert!(error.to_string().contains("unknown field `comand`"));
    }

    #[test]
    fn local_values_override_base_values_recursively() {
        let base = yaml_serde::from_str("service:\n  command: base\n  port: 3000\n").unwrap();
        let local = yaml_serde::from_str("service:\n  command: local\n").unwrap();
        let merged = deep_merge(base, local);

        assert_eq!(merged["service"]["command"], "local");
        assert_eq!(merged["service"]["port"], 3000);
    }

    #[test]
    fn loaded_unknown_field_error_includes_file_position_and_hint() {
        let root = std::env::temp_dir().join(format!("hum-schema-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(CONFIG_FILE);
        fs::write(
            &path,
            "version: 2\nproject: demo\n# comand: this comment is not the field\nservices:\n  api:\n    command: run\n    comand: typo\ntemplates:\n  all:\n    services: [api]\n",
        )
        .unwrap();

        let error = load(Some(&path)).unwrap_err().to_string();
        assert!(error.contains("line 7, column 5"), "{error}");
        assert!(error.contains("rename or remove"), "{error}");
        assert!(error.contains("valid field names"), "{error}");

        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn local_errors_are_attributed_to_local_file() {
        let root = std::env::temp_dir().join(format!("hum-local-error-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let base = root.join(CONFIG_FILE);
        let local = root.join(LOCAL_CONFIG_FILE);
        fs::write(
            &base,
            "version: 2\nproject: demo\nservices:\n  api:\n    command: run\ntemplates:\n  all:\n    services: [api]\n",
        )
        .unwrap();
        fs::write(&local, "services:\n  api:\n    port: 0\n").unwrap();

        let error = load(Some(&base)).unwrap_err().to_string();
        assert!(error.contains(&local.display().to_string()), "{error}");
        assert!(error.contains("services.api.port"), "{error}");

        fs::remove_file(base).unwrap();
        fs::remove_file(local).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn unchanged_base_error_stays_attributed_to_base_file() {
        let root = std::env::temp_dir().join(format!("hum-base-error-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let base = root.join(CONFIG_FILE);
        let local = root.join(LOCAL_CONFIG_FILE);
        fs::write(
            &base,
            "version: 2\nproject: demo\nservices:\n  api:\n    url: not-a-url\ntemplates:\n  all:\n    services: [api]\n",
        )
        .unwrap();
        fs::write(&local, "services:\n  api:\n    command: run\n").unwrap();

        let error = load(Some(&base)).unwrap_err().to_string();
        assert!(error.contains(&base.display().to_string()), "{error}");
        assert!(!error.contains(&local.display().to_string()), "{error}");
        assert!(error.contains("services.api.url"), "{error}");

        fs::remove_file(base).unwrap();
        fs::remove_file(local).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn full_environment_precedence_uses_merged_config() {
        let root = std::env::temp_dir().join(format!("hum-env-chain-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let base = root.join(CONFIG_FILE);
        let local = root.join(LOCAL_CONFIG_FILE);
        fs::write(
            &base,
            "version: 2\nproject: demo\nservices:\n  api:\n    command: run\n    env_file: service.env\n    env:\n      LAYER: base\n      BASE_ONLY: yes\ntemplates:\n  all:\n    services: [api]\n",
        )
        .unwrap();
        fs::write(
            &local,
            "services:\n  api:\n    env:\n      LAYER: local\n      LOCAL_ONLY: yes\n",
        )
        .unwrap();
        fs::write(root.join("service.env"), "LAYER=file\nFILE_ONLY=yes\n").unwrap();

        let inherited_key = format!("HUM_LAYER_{}", std::process::id());
        std::env::set_var(&inherited_key, "inherited");
        let mut loaded = load(Some(&base)).unwrap();
        let service = loaded.config.services.get_mut("api").unwrap();
        service
            .env
            .insert(inherited_key.clone(), "local".to_string());
        let inherited = crate::config::environment::resolve_service_env(
            service,
            &loaded.root_dir,
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(inherited[&inherited_key], "inherited");
        assert_eq!(inherited["LAYER"], "local");
        assert_eq!(inherited["FILE_ONLY"], "yes");
        assert_eq!(inherited["BASE_ONLY"], "yes");
        assert_eq!(inherited["LOCAL_ONLY"], "yes");

        let cli = HashMap::from([("LAYER".to_string(), "cli".to_string())]);
        let resolved =
            crate::config::environment::resolve_service_env(service, &loaded.root_dir, &cli)
                .unwrap();
        assert_eq!(resolved["LAYER"], "cli");
        std::env::remove_var(inherited_key);

        fs::remove_file(root.join("service.env")).unwrap();
        fs::remove_file(base).unwrap();
        fs::remove_file(local).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
