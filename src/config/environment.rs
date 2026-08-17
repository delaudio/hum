use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use super::{
    Config, EnvironmentProviderConfig, EnvironmentSourceConfig, ServiceConfig, TaskConfig,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnvironmentSyncReport {
    pub refreshed: usize,
    pub cached: usize,
    pub unavailable: usize,
}

#[derive(Clone)]
enum ProviderSourceOutcome {
    Fetched(HashMap<String, String>),
    Cached(HashMap<String, String>),
    Unavailable,
}

#[derive(Clone)]
enum ProviderReadOutcome {
    Fetched(String),
    Unavailable,
}

static PROVIDER_READ_CACHE: OnceLock<Mutex<HashMap<String, ProviderReadOutcome>>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
#[error(
    "required environment source from provider '{provider}' is unavailable and has no valid cache"
)]
struct OnePasswordProviderUnavailable {
    provider: String,
}

impl ProviderSourceOutcome {
    fn into_values(self) -> HashMap<String, String> {
        match self {
            Self::Fetched(values) | Self::Cached(values) => values,
            Self::Unavailable => HashMap::new(),
        }
    }
}

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
    let resolved = base_service_env(service, cwd)?;
    Ok(apply_runtime_overrides(resolved, cli_overrides))
}

/// Resolve provider-backed sources for a service during startup. Provider
/// values are scoped to the selected service and never mutate hum's process.
pub async fn resolve_service_env_with_providers(
    config: &Config,
    service: &ServiceConfig,
    cwd: &Path,
    project_root: &Path,
    cli_overrides: &HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let mut resolved = base_service_env(service, cwd)?;
    let mut provider_keys = HashSet::new();
    for source in &service.env_from {
        if provider_source_fully_shadowed(source, project_root, &service.env_overrides)? {
            continue;
        }
        let provider = config
            .environment_providers
            .get(&source.provider)
            .ok_or_else(|| anyhow!("unknown environment provider '{}'", source.provider))?
            .clone();
        let source = source.clone();
        let project_root = project_root.to_path_buf();
        let values = tokio::task::spawn_blocking(move || {
            resolve_provider_source(&provider, &source, &project_root)
        })
        .await
        .context("environment provider task failed")??;
        provider_keys.extend(values.keys().cloned());
        resolved.extend(values);
    }
    let mut shadowed = service
        .env_overrides
        .keys()
        .filter(|key| provider_keys.contains(*key))
        .cloned()
        .collect::<Vec<_>>();
    shadowed.sort();
    if !shadowed.is_empty() {
        eprintln!(
            "warning: env_overrides replace provider-backed keys: {}",
            shadowed.join(", ")
        );
    }
    apply_declared_overrides(&mut resolved, service);
    Ok(apply_runtime_overrides(resolved, cli_overrides))
}

fn apply_declared_overrides(resolved: &mut HashMap<String, String>, service: &ServiceConfig) {
    resolved.extend(service.env_overrides.clone());
}

pub async fn resolve_task_env_with_providers(
    config: &Config,
    task: &TaskConfig,
    cwd: &Path,
    project_root: &Path,
    cli_overrides: &HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let service = ServiceConfig {
        env: task.env.clone(),
        env_from: task.env_from.clone(),
        ..ServiceConfig::default()
    };
    resolve_service_env_with_providers(config, &service, cwd, project_root, cli_overrides).await
}

/// Refresh a set of provider-backed environment sources without exposing their
/// contents. Duplicate references shared by multiple units are fetched once.
pub async fn sync_environment_sources(
    config: &Config,
    sources: &[EnvironmentSourceConfig],
    project_root: &Path,
) -> Result<EnvironmentSyncReport> {
    let mut report = EnvironmentSyncReport::default();
    let mut seen = HashSet::new();
    let mut sources = sources.iter().collect::<Vec<_>>();
    // A task and service may declare the same reference with different
    // optionality. Process required declarations first so de-duplication keeps
    // the fail-closed contract for the whole selection.
    sources.sort_by_key(|source| source.optional);
    for source in sources {
        let identity = (
            source.provider.clone(),
            source.reference.clone(),
            source.args.clone(),
            source.schema.clone(),
            source.cache.clone(),
        );
        if !seen.insert(identity) {
            continue;
        }
        let provider = config
            .environment_providers
            .get(&source.provider)
            .ok_or_else(|| anyhow!("unknown environment provider '{}'", source.provider))?
            .clone();
        let source = source.clone();
        let project_root = project_root.to_path_buf();
        let outcome = tokio::task::spawn_blocking(move || {
            resolve_provider_source_outcome(&provider, &source, &project_root)
        })
        .await
        .context("environment provider sync task failed")??;
        match outcome {
            ProviderSourceOutcome::Fetched(_) => report.refreshed += 1,
            ProviderSourceOutcome::Cached(_) => report.cached += 1,
            ProviderSourceOutcome::Unavailable => report.unavailable += 1,
        }
    }
    Ok(report)
}

fn base_service_env(service: &ServiceConfig, cwd: &Path) -> Result<HashMap<String, String>> {
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
    Ok(resolved)
}

fn apply_runtime_overrides(
    mut resolved: HashMap<String, String>,
    cli_overrides: &HashMap<String, String>,
) -> HashMap<String, String> {
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
    resolved
}

fn resolve_provider_source(
    provider: &EnvironmentProviderConfig,
    source: &EnvironmentSourceConfig,
    project_root: &Path,
) -> Result<HashMap<String, String>> {
    resolve_provider_source_outcome(provider, source, project_root)
        .map(|outcome| outcome.into_values())
}

fn resolve_provider_source_outcome(
    provider: &EnvironmentProviderConfig,
    source: &EnvironmentSourceConfig,
    project_root: &Path,
) -> Result<ProviderSourceOutcome> {
    let outcome =
        read_provider_reference_cached(provider, source.reference.as_deref(), &source.args);
    resolve_provider_source_outcome_from_read(source, project_root, outcome)
}

fn provider_cache_key(
    provider: &EnvironmentProviderConfig,
    reference: Option<&str>,
    args: &[String],
) -> String {
    match provider {
        EnvironmentProviderConfig::OnePassword { account } => {
            format!(
                "one-password\0{}\0{}",
                account.as_deref().unwrap_or(""),
                reference.unwrap_or("")
            )
        }
        EnvironmentProviderConfig::Exec { command } => {
            format!(
                "exec\0{}\0{}\0{}",
                command.join("\u{1f}"),
                reference.unwrap_or(""),
                args.join("\u{1f}")
            )
        }
    }
}

fn read_provider_reference_cached(
    provider: &EnvironmentProviderConfig,
    reference: Option<&str>,
    args: &[String],
) -> ProviderReadOutcome {
    let key = provider_cache_key(provider, reference, args);
    let cache = PROVIDER_READ_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(outcome) = cache.get(&key) {
        return outcome.clone();
    }
    let outcome = match provider {
        EnvironmentProviderConfig::OnePassword { account } => {
            let reference = reference.unwrap_or_default();
            match read_one_password_reference(reference, account.as_deref()) {
                Ok(contents) => ProviderReadOutcome::Fetched(contents),
                Err(_) => ProviderReadOutcome::Unavailable,
            }
        }
        EnvironmentProviderConfig::Exec { command } => match read_exec_command(command, args) {
            Ok(contents) => ProviderReadOutcome::Fetched(contents),
            Err(_) => ProviderReadOutcome::Unavailable,
        },
    };
    cache.insert(key, outcome.clone());
    outcome
}

pub fn clear_provider_read_cache() {
    if let Some(cache) = PROVIDER_READ_CACHE.get() {
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

pub fn is_one_password_unavailable(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<OnePasswordProviderUnavailable>())
}

pub fn sign_in_one_password() -> Result<bool> {
    let status = Command::new("op")
        .arg("signin")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|_| anyhow!("1Password CLI is unavailable or could not be started"))?;
    Ok(status.success())
}

pub fn one_password_session_available() -> Result<bool> {
    let status = Command::new("op")
        .arg("whoami")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|_| anyhow!("1Password CLI is unavailable or could not be started"))?;
    Ok(status.success())
}

#[cfg(test)]
fn resolve_provider_source_with<F>(
    provider: &EnvironmentProviderConfig,
    source: &EnvironmentSourceConfig,
    project_root: &Path,
    read: F,
) -> Result<HashMap<String, String>>
where
    F: FnOnce(&EnvironmentProviderConfig, Option<&str>) -> Result<String>,
{
    let outcome = match read(provider, source.reference.as_deref()) {
        Ok(contents) => ProviderReadOutcome::Fetched(contents),
        Err(_) => ProviderReadOutcome::Unavailable,
    };
    resolve_provider_source_outcome_from_read(source, project_root, outcome)
        .map(ProviderSourceOutcome::into_values)
}

fn resolve_provider_source_outcome_from_read(
    source: &EnvironmentSourceConfig,
    project_root: &Path,
    outcome: ProviderReadOutcome,
) -> Result<ProviderSourceOutcome> {
    match outcome {
        ProviderReadOutcome::Fetched(contents) => {
            match parse_and_validate_provider_dotenv(&contents, source, project_root) {
                Ok(values) => {
                    if let Some(cache) = &source.cache {
                        write_private_cache_atomic(&project_root.join(cache), &contents)?;
                    }
                    Ok(ProviderSourceOutcome::Fetched(values))
                }
                Err(error) => match read_valid_cache(source, project_root) {
                    Ok(values) => Ok(ProviderSourceOutcome::Cached(values)),
                    Err(_) if source.optional => Ok(ProviderSourceOutcome::Unavailable),
                    Err(_) => Err(error),
                },
            }
        }
        ProviderReadOutcome::Unavailable => match read_valid_cache(source, project_root) {
            Ok(values) => Ok(ProviderSourceOutcome::Cached(values)),
            Err(_) if source.optional => Ok(ProviderSourceOutcome::Unavailable),
            Err(_) => Err(OnePasswordProviderUnavailable {
                provider: source.provider.clone(),
            }
            .into()),
        },
    }
}

fn read_one_password_reference(reference: &str, account: Option<&str>) -> Result<String> {
    let mut command = Command::new("op");
    command.arg("read").arg("--no-newline");
    if let Some(account) = account.filter(|account| !account.is_empty()) {
        command.arg("--account").arg(account);
    }
    let output = command
        .arg(reference)
        .output()
        .map_err(|_| anyhow!("1Password CLI is unavailable or could not be started"))?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(anyhow!("1Password could not resolve the requested item"));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| anyhow!("1Password returned a non-UTF-8 environment"))
}

fn read_exec_command(command: &[String], args: &[String]) -> Result<String> {
    let (program, base_args) = command
        .split_first()
        .ok_or_else(|| anyhow!("exec environment provider has an empty command"))?;
    let output = Command::new(program)
        .args(base_args)
        .args(args)
        .output()
        .map_err(|_| {
            anyhow!("exec environment provider command is unavailable or could not be started")
        })?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(anyhow!(
            "exec environment provider command did not resolve any output"
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| anyhow!("exec environment provider returned a non-UTF-8 environment"))
}

fn parse_and_validate_provider_dotenv(
    contents: &str,
    source: &EnvironmentSourceConfig,
    project_root: &Path,
) -> Result<HashMap<String, String>> {
    let values = dotenvy::from_read_iter(contents.as_bytes())
        .collect::<std::result::Result<HashMap<_, _>, _>>()
        .map_err(|_| anyhow!("environment provider returned invalid dotenv"))?;
    if values.is_empty() {
        return Err(anyhow!("environment provider returned an empty dotenv"));
    }
    if let Some(schema) = &source.schema {
        let schema_path = project_root.join(schema);
        let allowed = read_environment_schema(&schema_path)?;
        let mut actual = values.keys().collect::<Vec<_>>();
        let mut expected = allowed.keys().collect::<Vec<_>>();
        actual.sort();
        expected.sort();
        if actual != expected {
            return Err(anyhow!(
                "environment provider keys do not match schema {}",
                schema_path.display()
            ));
        }
    }
    Ok(values)
}

fn read_environment_schema(path: &Path) -> Result<HashMap<String, String>> {
    dotenvy::from_path_iter(path)
        .with_context(|| format!("failed to read environment schema {}", path.display()))?
        .collect::<std::result::Result<HashMap<_, _>, _>>()
        .map_err(|_| anyhow!("failed to parse environment schema {}", path.display()))
}

fn provider_source_fully_shadowed(
    source: &EnvironmentSourceConfig,
    project_root: &Path,
    overrides: &HashMap<String, String>,
) -> Result<bool> {
    let Some(schema) = &source.schema else {
        return Ok(false);
    };
    let keys = read_environment_schema(&project_root.join(schema))?
        .into_keys()
        .collect::<Vec<_>>();
    Ok(!keys.is_empty() && keys.iter().all(|key| overrides.contains_key(key)))
}

fn read_valid_cache(
    source: &EnvironmentSourceConfig,
    project_root: &Path,
) -> Result<HashMap<String, String>> {
    let cache = source
        .cache
        .as_ref()
        .ok_or_else(|| anyhow!("no environment provider cache is configured"))?;
    let contents = fs::read_to_string(project_root.join(cache))
        .map_err(|_| anyhow!("environment provider cache is unavailable"))?;
    parse_and_validate_provider_dotenv(&contents, source, project_root)
}

fn write_private_cache_atomic(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("environment cache path has no parent directory"))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create environment cache directory {}",
            parent.display()
        )
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".hum-env-{}-{nonce}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).with_context(|| {
            format!("failed to create environment cache near {}", path.display())
        })?;
        file.write_all(contents.as_bytes())
            .context("failed to write environment cache")?;
        file.sync_all()
            .context("failed to sync environment cache")?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace environment cache {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
                format!("failed to protect environment cache {}", path.display())
            })?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::config::{EnvironmentSourceFormat, RuntimeConfig};

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

    fn provider_source(optional: bool) -> EnvironmentSourceConfig {
        EnvironmentSourceConfig {
            provider: "company".to_string(),
            reference: Some("op://Development/api/environment".to_string()),
            format: EnvironmentSourceFormat::Dotenv,
            optional,
            schema: Some("api.env.example".into()),
            cache: Some(".hum/cache/api.env".into()),
            args: Vec::new(),
        }
    }

    #[test]
    fn provider_dotenv_is_validated_and_cached_privately() {
        let temp = std::env::temp_dir().join(format!(
            "hum-provider-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("api.env.example"), "PUBLIC_URL=\nSECRET=\n").unwrap();
        let provider = EnvironmentProviderConfig::OnePassword { account: None };
        let source = provider_source(false);
        let values = resolve_provider_source_with(&provider, &source, &temp, |_, _| {
            Ok("PUBLIC_URL=http://localhost\nSECRET=fetched\n".to_string())
        })
        .unwrap();

        assert_eq!(values["SECRET"], "fetched");
        let cache = temp.join(".hum/cache/api.env");
        assert!(fs::read_to_string(&cache).unwrap().contains("fetched"));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn optional_provider_failure_uses_only_a_schema_valid_cache() {
        let temp = std::env::temp_dir().join(format!(
            "hum-provider-cache-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(temp.join(".hum/cache")).unwrap();
        fs::write(temp.join("api.env.example"), "PUBLIC_URL=\nSECRET=\n").unwrap();
        fs::write(
            temp.join(".hum/cache/api.env"),
            "PUBLIC_URL=http://cached\nSECRET=cached\n",
        )
        .unwrap();
        let provider = EnvironmentProviderConfig::OnePassword { account: None };
        let values =
            resolve_provider_source_with(&provider, &provider_source(true), &temp, |_, _| {
                Err(anyhow!("must-not-leak-provider-detail"))
            })
            .unwrap();
        assert_eq!(values["SECRET"], "cached");

        fs::write(temp.join(".hum/cache/api.env"), "UNDECLARED=value\n").unwrap();
        let values =
            resolve_provider_source_with(&provider, &provider_source(true), &temp, |_, _| {
                Err(anyhow!("must-not-leak-provider-detail"))
            })
            .unwrap();
        assert!(values.is_empty());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn required_provider_failure_reuses_a_valid_cache_but_fails_without_one() {
        let temp = std::env::temp_dir().join(format!(
            "hum-required-provider-cache-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(temp.join(".hum/cache")).unwrap();
        fs::write(temp.join("api.env.example"), "PUBLIC_URL=\nSECRET=\n").unwrap();
        fs::write(
            temp.join(".hum/cache/api.env"),
            "PUBLIC_URL=http://cached\nSECRET=cached\n",
        )
        .unwrap();
        let provider = EnvironmentProviderConfig::OnePassword { account: None };
        let source = provider_source(false);
        let values = resolve_provider_source_with(&provider, &source, &temp, |_, _| {
            Err(anyhow!("provider unavailable"))
        })
        .unwrap();
        assert_eq!(values["SECRET"], "cached");

        fs::write(temp.join(".hum/cache/api.env"), "UNDECLARED=value\n").unwrap();
        let error = resolve_provider_source_with(&provider, &source, &temp, |_, _| {
            Err(anyhow!("provider unavailable"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("no valid cache"));
        assert!(is_one_password_unavailable(&error));

        let validation_error = resolve_provider_source_with(&provider, &source, &temp, |_, _| {
            Ok("UNDECLARED=fetched\n".to_string())
        })
        .unwrap_err();
        assert!(!is_one_password_unavailable(&validation_error));
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn fully_shadowed_provider_schema_does_not_require_provider_resolution() {
        let temp = std::env::temp_dir().join(format!(
            "hum-provider-shadow-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("api.env.example"), "PUBLIC_URL=\nSECRET=\n").unwrap();
        let source = provider_source(false);
        let all = HashMap::from([
            ("PUBLIC_URL".to_string(), "http://localhost".to_string()),
            ("SECRET".to_string(), "local-only".to_string()),
        ]);
        assert!(provider_source_fully_shadowed(&source, &temp, &all).unwrap());
        let partial = HashMap::from([("PUBLIC_URL".to_string(), "http://localhost".to_string())]);
        assert!(!provider_source_fully_shadowed(&source, &temp, &partial).unwrap());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn exec_provider_captures_command_stdout() {
        let command = vec![
            "/bin/echo".to_string(),
            "-n".to_string(),
            "GREETING=hello".to_string(),
        ];
        let contents = read_exec_command(&command, &[]).unwrap();
        assert_eq!(contents, "GREETING=hello");
    }

    #[test]
    fn exec_provider_appends_source_args_to_base_command() {
        let command = vec!["/bin/echo".to_string(), "-n".to_string()];
        let args = vec!["REPO=compri-applications".to_string()];
        let contents = read_exec_command(&command, &args).unwrap();
        assert_eq!(contents, "REPO=compri-applications");
    }

    #[test]
    fn exec_provider_failure_is_unavailable() {
        let command = vec!["/usr/bin/false".to_string()];
        assert!(read_exec_command(&command, &[]).is_err());
    }

    #[test]
    fn exec_provider_cache_key_distinguishes_args() {
        let provider = EnvironmentProviderConfig::Exec {
            command: vec!["/bin/echo".to_string()],
        };
        let with_repo_a = provider_cache_key(&provider, None, &["repo-a".to_string()]);
        let with_repo_b = provider_cache_key(&provider, None, &["repo-b".to_string()]);
        assert_ne!(with_repo_a, with_repo_b);
    }

    #[test]
    fn exec_provider_read_is_cached_per_args() {
        let provider = EnvironmentProviderConfig::Exec {
            command: vec!["/bin/echo".to_string(), "-n".to_string()],
        };
        let a = read_provider_reference_cached(&provider, None, &["VALUE=a".to_string()]);
        let b = read_provider_reference_cached(&provider, None, &["VALUE=b".to_string()]);
        match (a, b) {
            (ProviderReadOutcome::Fetched(a), ProviderReadOutcome::Fetched(b)) => {
                assert_eq!(a, "VALUE=a");
                assert_eq!(b, "VALUE=b");
            }
            _ => panic!("expected both exec provider reads to succeed"),
        }
    }

    #[test]
    fn v3_provider_resolution_keeps_inherited_and_cli_precedence() {
        let service = ServiceConfig {
            runtime: Some("local".to_string()),
            env: HashMap::from([("SHARED".to_string(), "literal".to_string())]),
            env_overrides: HashMap::from([("SHARED".to_string(), "runtime-overlay".to_string())]),
            ..ServiceConfig::default()
        };
        let config = Config {
            version: 3,
            runtimes: HashMap::from([("local".to_string(), RuntimeConfig::Process {})]),
            ..Config::default()
        };
        let base = base_service_env(&service, Path::new(".")).unwrap();
        assert_eq!(base["SHARED"], "literal");
        assert!(config.runtimes.contains_key("local"));
        let mut provider_values = HashMap::from([("SHARED".to_string(), "provider".to_string())]);
        apply_declared_overrides(&mut provider_values, &service);
        assert_eq!(provider_values["SHARED"], "runtime-overlay");
        let resolved = apply_runtime_overrides(
            provider_values,
            &HashMap::from([("SHARED".to_string(), "cli".to_string())]),
        );
        assert_eq!(resolved["SHARED"], "cli");
    }
}
