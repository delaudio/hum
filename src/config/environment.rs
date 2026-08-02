use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

use super::ServiceConfig;

/// Parse repeatable CLI assignments such as `--env API_URL=http://localhost`.
pub fn parse_overrides(values: &[String]) -> Result<HashMap<String, String>> {
    values
        .iter()
        .map(|value| {
            let (key, value) = value
                .split_once('=')
                .ok_or_else(|| anyhow!("invalid --env value; expected KEY=VALUE"))?;
            if key.trim().is_empty() {
                return Err(anyhow!("invalid --env value; KEY cannot be empty"));
            }
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Resolve a service environment without mutating the parent process.
/// Precedence, lowest to highest: env file, service YAML, inherited process,
/// explicit CLI overrides.
pub fn resolve_service_env(
    service: &ServiceConfig,
    cwd: &Path,
    cli_overrides: &HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let mut resolved = match &service.env_file {
        Some(path) => {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            };
            let values = dotenvy::from_path_iter(&path)
                .with_context(|| format!("failed to read env file {}", path.display()))?;
            values
                .collect::<std::result::Result<HashMap<_, _>, _>>()
                .map_err(|_| anyhow!("failed to parse env file {}", path.display()))?
        }
        None => HashMap::new(),
    };

    resolved.extend(service.env.clone());

    // Inherited values only need to replace keys already declared for the
    // service. All other process variables are inherited by Command itself.
    for key in resolved.keys().cloned().collect::<Vec<_>>() {
        if let Some(value) = std::env::var_os(&key) {
            match value.into_string() {
                Ok(value) => {
                    resolved.insert(key, value);
                }
                Err(_) => {
                    // Do not overwrite a non-UTF-8 inherited value with the
                    // lower-priority YAML/env-file declaration.
                    resolved.remove(&key);
                }
            }
        }
    }

    resolved.extend(cli_overrides.clone());
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn parses_values_containing_equals() {
        let values = vec!["TOKEN=abc=123".to_string()];
        assert_eq!(parse_overrides(&values).unwrap()["TOKEN"], "abc=123");
    }

    #[test]
    fn rejects_invalid_assignment() {
        let secret = "do-not-print";
        for input in [format!("TOKEN-{secret}"), format!("={secret}")] {
            let error = parse_overrides(&[input]).unwrap_err().to_string();
            assert!(!error.contains(secret));
        }
    }

    #[test]
    fn env_file_is_lower_priority_than_service_and_cli() {
        let temp = std::env::temp_dir().join(format!("hum-env-test-{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("service.env"), "FROM_FILE=yes\nSHARED=file\n").unwrap();

        let service = ServiceConfig {
            env_file: Some("service.env".into()),
            env: HashMap::from([
                ("SHARED".to_string(), "service".to_string()),
                ("SERVICE_ONLY".to_string(), "yes".to_string()),
            ]),
            ..ServiceConfig::default()
        };
        let overrides = HashMap::from([("SHARED".to_string(), "cli".to_string())]);
        let resolved = resolve_service_env(&service, &temp, &overrides).unwrap();

        assert_eq!(resolved["FROM_FILE"], "yes");
        assert_eq!(resolved["SERVICE_ONLY"], "yes");
        assert_eq!(resolved["SHARED"], "cli");

        fs::remove_file(temp.join("service.env")).unwrap();
        fs::remove_dir(temp).unwrap();
    }

    #[test]
    fn inherited_environment_overrides_declared_values() {
        let key = format!("HUM_TEST_INHERITED_{}", std::process::id());
        std::env::set_var(&key, "inherited");
        let service = ServiceConfig {
            env: HashMap::from([(key.clone(), "service".to_string())]),
            ..ServiceConfig::default()
        };

        let resolved = resolve_service_env(&service, Path::new("."), &HashMap::new()).unwrap();
        assert_eq!(resolved[&key], "inherited");
        std::env::remove_var(key);
    }

    #[test]
    fn parse_errors_do_not_expose_secret_values() {
        let temp = std::env::temp_dir().join(format!("hum-secret-test-{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("service.env"), "TOKEN='do-not-print\n").unwrap();
        let service = ServiceConfig {
            env_file: Some("service.env".into()),
            ..ServiceConfig::default()
        };

        let error = resolve_service_env(&service, &temp, &HashMap::new()).unwrap_err();
        assert!(!error.to_string().contains("do-not-print"));

        fs::remove_file(temp.join("service.env")).unwrap();
        fs::remove_dir(temp).unwrap();
    }
}
