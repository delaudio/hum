use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

use super::error::ConfigError;
use super::model::{Config, EnvironmentProviderConfig, HealthcheckConfig, RuntimeConfig};

const MAX_REDACT_PATTERNS: usize = 128;
const MAX_REDACT_PATTERN_BYTES: usize = 256 * 1024;
const MAX_LOG_EXPORTERS: usize = 16;
const MAX_LOG_EXPORT_ENDPOINT_BYTES: usize = 8 * 1024;
const MAX_LOG_EXPORT_HEADERS: usize = 16;
const MAX_LOG_EXPORT_HEADER_BYTES: usize = 8 * 1024;

/// Validate a fully-merged configuration. Catches everything that can be
/// checked statically: version, dangling references and dependency cycles.
pub fn validate(config: &Config, file: &Path) -> Result<(), ConfigError> {
    if !matches!(config.version, 1..=3) {
        return Err(ConfigError::validation(
            file,
            "version",
            format!("unsupported config version {}", config.version),
            "set `version: 2` for process-only projects or `version: 3` for runtime adapters and environment providers",
        ));
    }

    if config.version >= 2
        && config
            .project
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        return Err(ConfigError::validation(
            file,
            "project",
            format!(
                "version {} configuration requires a project identifier",
                config.version
            ),
            "add `project: <name>` matching the global registry entry",
        ));
    }
    if let Some(project) = config.project.as_deref() {
        if !is_safe_identifier(project) {
            return Err(ConfigError::validation(
                file,
                "project",
                "project identifier contains unsafe path characters",
                "use letters, numbers, dots, underscores, or hyphens and start with a letter or number",
            ));
        }
    }

    validate_names(config, file)?;
    validate_logs(config, file)?;
    validate_v3_contract(config, file)?;

    // Services can be selected explicitly and templates may overlap, so a port
    // must identify at most one service across the whole project.
    let mut configured_ports: HashMap<u16, &str> = HashMap::new();
    for (name, service) in &config.services {
        let process_service = config.version <= 2
            || service.runtime.as_deref().is_some_and(|runtime| {
                matches!(
                    config.runtimes.get(runtime),
                    Some(RuntimeConfig::Process {})
                )
            });
        if process_service
            && service
                .command
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
        {
            return Err(ConfigError::validation(
                file,
                format!("services.{name}.command"),
                "service has no command to run",
                format!("add a `command:` to services.{name}"),
            ));
        }

        if service.port == Some(0) {
            return Err(ConfigError::validation(
                file,
                format!("services.{name}.port"),
                "port must be between 1 and 65535",
                "choose the TCP port exposed by the service",
            ));
        }
        if let Some(port) = service.port {
            if let Some(other) = configured_ports.insert(port, name) {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.port"),
                    format!("port {port} is also assigned to service '{other}'"),
                    "assign a distinct port to each service",
                ));
            }
        }

        if let Some(url) = &service.url {
            validate_url(file, format!("services.{name}.url"), url)?;
        }

        if let Some(healthcheck) = &service.healthcheck {
            validate_healthcheck(file, name, healthcheck)?;
        }

        match service.depends_on_ready {
            Some(super::model::ReadyMode::Listening) if service.port.is_none() => {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.depends_on_ready"),
                    "listening readiness requires a configured service port",
                    format!("add `port:` to services.{name} or use `started` readiness"),
                ));
            }
            Some(super::model::ReadyMode::Healthy) if service.healthcheck.is_none() => {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.depends_on_ready"),
                    "healthy readiness requires a configured healthcheck",
                    format!("add `healthcheck:` to services.{name} or use `started` readiness"),
                ));
            }
            _ => {}
        }

        if let Some(repo) = &service.repository {
            if !config.repositories.contains_key(repo) {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.repository"),
                    format!("references unknown repository '{repo}'"),
                    format!(
                        "add `{repo}:` under `repositories:`, or fix the typo in services.{name}"
                    ),
                ));
            }
        }

        let mut dependencies = HashSet::new();
        for dep in &service.depends_on {
            if !dependencies.insert(dep) {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.depends_on"),
                    format!("dependency '{dep}' is listed more than once"),
                    "remove the duplicate dependency",
                ));
            }
            if dep == name {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.depends_on"),
                    "a service cannot depend on itself",
                    format!("remove '{dep}' from services.{name}.depends_on"),
                ));
            }
            if !(config.services.contains_key(dep)
                || config.version >= 3 && config.tasks.contains_key(dep))
            {
                return Err(ConfigError::validation(
                    file,
                    format!("services.{name}.depends_on"),
                    format!("depends on unknown service or task '{dep}'"),
                    format!("define `{dep}` under services or tasks, or fix the reference"),
                ));
            }
        }
    }

    for (name, template) in &config.templates {
        let mut selected = HashSet::new();
        for svc in &template.services {
            if !selected.insert(svc) {
                return Err(ConfigError::validation(
                    file,
                    format!("templates.{name}.services"),
                    format!("service '{svc}' is listed more than once"),
                    "remove the duplicate service entry",
                ));
            }
            if !config.services.contains_key(svc) {
                return Err(ConfigError::validation(
                    file,
                    format!("templates.{name}.services"),
                    format!("references unknown service '{svc}'"),
                    format!("define a `services.{svc}` entry, or fix the typo in templates.{name}"),
                ));
            }
        }
    }

    detect_cycles(config, file)?;

    Ok(())
}

fn validate_logs(config: &Config, file: &Path) -> Result<(), ConfigError> {
    if config.logs.max_file_bytes == 0 {
        return Err(ConfigError::validation(
            file,
            "logs.max_file_bytes",
            "maximum log file size must be greater than zero",
            "set max_file_bytes to a positive byte count",
        ));
    }
    if config.logs.rotated_files > 16 {
        return Err(ConfigError::validation(
            file,
            "logs.rotated_files",
            "at most 16 rotated files are supported per stream",
            "lower rotated_files to 16 or less",
        ));
    }
    if config.logs.max_line_bytes == 0 || config.logs.max_line_bytes > 16 * 1024 * 1024 {
        return Err(ConfigError::validation(
            file,
            "logs.max_line_bytes",
            "log line/chunk limit must be between 1 byte and 16 MiB",
            "set max_line_bytes to a bounded positive value, for example 65536",
        ));
    }
    if config.logs.redact_patterns.len() > MAX_REDACT_PATTERNS {
        return Err(ConfigError::validation(
            file,
            "logs.redact_patterns",
            format!("at most {MAX_REDACT_PATTERNS} redaction patterns are supported"),
            "remove redundant patterns or combine them into bounded expressions",
        ));
    }
    let redact_pattern_bytes = config
        .logs
        .redact_patterns
        .iter()
        .fold(0_usize, |total, pattern| {
            total.saturating_add(pattern.len())
        });
    if redact_pattern_bytes > MAX_REDACT_PATTERN_BYTES {
        return Err(ConfigError::validation(
            file,
            "logs.redact_patterns",
            "redaction patterns exceed the 256 KiB aggregate safety limit",
            "shorten or remove redundant redaction expressions",
        ));
    }
    for (index, pattern) in config.logs.redact_patterns.iter().enumerate() {
        if pattern.is_empty() {
            return Err(ConfigError::validation(
                file,
                format!("logs.redact_patterns.{index}"),
                "redaction pattern cannot be empty",
                "remove the empty entry or provide a regular expression",
            ));
        }
        if let Err(error) = regex::Regex::new(pattern) {
            return Err(ConfigError::validation(
                file,
                format!("logs.redact_patterns.{index}"),
                format!("invalid redaction regular expression: {error}"),
                "fix the regular expression syntax",
            ));
        }
    }
    if config.logs.exporters.len() > MAX_LOG_EXPORTERS {
        return Err(ConfigError::validation(
            file,
            "logs.exporters",
            format!("at most {MAX_LOG_EXPORTERS} log exporters are supported"),
            "remove duplicate exporters or route through one local collector",
        ));
    }
    for (index, exporter) in config.logs.exporters.iter().enumerate() {
        match exporter {
            crate::config::LogExporterConfig::Http {
                endpoint,
                timeout,
                headers,
            } => {
                if endpoint.len() > MAX_LOG_EXPORT_ENDPOINT_BYTES {
                    return Err(ConfigError::validation(
                        file,
                        format!("logs.exporters.{index}.endpoint"),
                        "HTTP log exporter endpoint exceeds the 8 KiB safety limit",
                        "use a shorter collector URL",
                    ));
                }
                let parsed = reqwest::Url::parse(endpoint).map_err(|error| {
                    ConfigError::validation(
                        file,
                        format!("logs.exporters.{index}.endpoint"),
                        format!("invalid HTTP log exporter endpoint: {error}"),
                        "use an absolute http:// or https:// URL",
                    )
                })?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
                    return Err(ConfigError::validation(
                        file,
                        format!("logs.exporters.{index}.endpoint"),
                        "HTTP log exporter endpoint must use http or https and include a host",
                        "use an absolute URL such as http://127.0.0.1:8687/events",
                    ));
                }
                let host = parsed.host_str().unwrap_or_default();
                let normalized_host = host.strip_suffix('.').unwrap_or(host);
                // reqwest::Url 0.12 retains brackets in host_str() for IPv6
                // literals (covered by the ::1 validation test below).
                let ip_host = normalized_host
                    .strip_prefix('[')
                    .and_then(|host| host.strip_suffix(']'))
                    .unwrap_or(normalized_host);
                // This is a syntactic configuration-safety check. Hum does
                // not resolve arbitrary hostnames to make trust decisions.
                let loopback_ip = ip_host.parse::<std::net::IpAddr>().is_ok_and(|address| {
                    address.is_loopback()
                        || matches!(
                            address,
                            std::net::IpAddr::V6(address)
                                if address
                                    .to_ipv4_mapped()
                                    .is_some_and(|address| address.is_loopback())
                        )
                });
                let loopback = normalized_host.eq_ignore_ascii_case("localhost")
                    || normalized_host.to_ascii_lowercase().ends_with(".localhost")
                    || loopback_ip;
                if parsed.scheme() == "http" && !loopback {
                    return Err(ConfigError::validation(
                        file,
                        format!("logs.exporters.{index}.endpoint"),
                        "non-loopback HTTP log exporter endpoints are not allowed",
                        "use HTTPS for remote collectors or a loopback HTTP endpoint for local tools",
                    ));
                }
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    return Err(ConfigError::validation(
                        file,
                        format!("logs.exporters.{index}.endpoint"),
                        "HTTP log exporter endpoint must not embed credentials",
                        "use a credential-free local endpoint",
                    ));
                }
                if parsed.query().is_some() || parsed.fragment().is_some() {
                    return Err(ConfigError::validation(
                        file,
                        format!("logs.exporters.{index}.endpoint"),
                        "HTTP log exporter endpoint must not include a query or fragment",
                        "put routing information in the URL path and keep credentials out of configuration",
                    ));
                }
                if timeout.is_zero() || *timeout > std::time::Duration::from_secs(10) {
                    return Err(ConfigError::validation(
                        file,
                        format!("logs.exporters.{index}.timeout"),
                        "HTTP log exporter timeout must be greater than zero and at most 10 seconds",
                        "use a short best-effort timeout such as 750ms",
                    ));
                }
                if headers.len() > MAX_LOG_EXPORT_HEADERS {
                    return Err(ConfigError::validation(
                        file,
                        format!("logs.exporters.{index}.headers"),
                        format!("at most {MAX_LOG_EXPORT_HEADERS} HTTP headers are supported"),
                        "remove unnecessary exporter headers",
                    ));
                }
                let mut header_bytes = 0_usize;
                for (name, value) in headers {
                    header_bytes = header_bytes
                        .saturating_add(name.len())
                        .saturating_add(value.len());
                    if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
                        return Err(ConfigError::validation(
                            file,
                            format!("logs.exporters.{index}.headers.{name}"),
                            "invalid HTTP header name",
                            "use an RFC-compliant HTTP header name",
                        ));
                    }
                    if reqwest::header::HeaderValue::from_str(value).is_err() {
                        return Err(ConfigError::validation(
                            file,
                            format!("logs.exporters.{index}.headers.{name}"),
                            "invalid HTTP header value",
                            "remove control characters from the header value",
                        ));
                    }
                }
                if header_bytes > MAX_LOG_EXPORT_HEADER_BYTES {
                    return Err(ConfigError::validation(
                        file,
                        format!("logs.exporters.{index}.headers"),
                        "HTTP log exporter headers exceed the aggregate 8 KiB safety limit",
                        "use smaller header values",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_names(config: &Config, file: &Path) -> Result<(), ConfigError> {
    for (namespace, names) in [
        (
            "repositories",
            config.repositories.keys().collect::<Vec<_>>(),
        ),
        ("services", config.services.keys().collect::<Vec<_>>()),
        ("tasks", config.tasks.keys().collect::<Vec<_>>()),
        ("templates", config.templates.keys().collect::<Vec<_>>()),
        ("runtimes", config.runtimes.keys().collect::<Vec<_>>()),
        (
            "environment_providers",
            config.environment_providers.keys().collect::<Vec<_>>(),
        ),
    ] {
        let mut normalized = HashMap::new();
        for name in names {
            if name.trim().is_empty() {
                return Err(ConfigError::validation(
                    file,
                    namespace,
                    "names cannot be empty",
                    "use a stable non-empty identifier",
                ));
            }
            if !is_safe_identifier(name) {
                return Err(ConfigError::validation(
                    file,
                    namespace,
                    format!("name '{name}' contains unsafe path characters"),
                    "use letters, numbers, dots, underscores, or hyphens and start with a letter or number",
                ));
            }
            let folded = name.to_lowercase();
            if let Some(other) = normalized.insert(folded, name) {
                return Err(ConfigError::validation(
                    file,
                    namespace,
                    format!("names '{other}' and '{name}' collide when case is ignored"),
                    "rename one entry so identifiers are unique across platforms",
                ));
            }
        }
    }
    Ok(())
}

fn validate_v3_contract(config: &Config, file: &Path) -> Result<(), ConfigError> {
    if config.version <= 2 {
        if !config.runtimes.is_empty()
            || !config.environment_providers.is_empty()
            || !config.tasks.is_empty()
        {
            return Err(ConfigError::validation(
                file,
                "version",
                "runtime adapters and environment providers require configuration version 3",
                "set `version: 3`, or remove the v3-only fields",
            ));
        }
        if let Some((name, _)) = config.services.iter().find(|(_, service)| {
            service.runtime.is_some()
                || service.target.is_some()
                || !service.env_from.is_empty()
                || !service.env_overrides.is_empty()
        }) {
            return Err(ConfigError::validation(
                file,
                format!("services.{name}"),
                "runtime, target, env_from, and env_overrides require configuration version 3",
                "set `version: 3`, or remove the v3-only service fields",
            ));
        }
        return Ok(());
    }

    if config.runtimes.is_empty() {
        return Err(ConfigError::validation(
            file,
            "runtimes",
            "version 3 configuration has no runtime adapters",
            "declare at least one named process or compose runtime",
        ));
    }

    if let Some(name) = config
        .tasks
        .keys()
        .find(|name| config.services.contains_key(*name))
    {
        return Err(ConfigError::validation(
            file,
            "tasks",
            format!("unit name '{name}' is used by both a service and a task"),
            "use unique names across services and tasks",
        ));
    }

    for (name, runtime) in &config.runtimes {
        if let RuntimeConfig::Compose {
            project_name,
            files,
            generated_files,
            profiles,
            ..
        } = runtime
        {
            if !is_compose_project_name(project_name) {
                return Err(ConfigError::validation(
                    file,
                    format!("runtimes.{name}.project_name"),
                    "invalid Docker Compose project name",
                    "use lowercase letters, numbers, hyphens, or underscores and start with a letter or number",
                ));
            }
            if files.is_empty() {
                return Err(ConfigError::validation(
                    file,
                    format!("runtimes.{name}.files"),
                    "compose runtime requires at least one Compose file",
                    "add a path such as `compose.yaml`",
                ));
            }
            let mut compose_files = HashSet::new();
            if files
                .iter()
                .chain(generated_files)
                .any(|path| path.as_os_str().is_empty() || !compose_files.insert(path))
            {
                return Err(ConfigError::validation(
                    file,
                    format!("runtimes.{name}.files"),
                    "Compose file paths must be non-empty and unique across files and generated_files",
                    "remove empty or duplicate Compose file paths",
                ));
            }
            let mut unique_profiles = HashSet::new();
            if profiles
                .iter()
                .any(|profile| profile.trim().is_empty() || !unique_profiles.insert(profile))
            {
                return Err(ConfigError::validation(
                    file,
                    format!("runtimes.{name}.profiles"),
                    "compose profiles must be non-empty and unique",
                    "remove empty or duplicate profile entries",
                ));
            }
        }
    }

    validate_environment_providers(config, file)?;

    let mut compose_targets = HashSet::new();
    for (name, service) in &config.services {
        let runtime_name = service.runtime.as_deref().ok_or_else(|| {
            ConfigError::validation(
                file,
                format!("services.{name}.runtime"),
                "version 3 service has no runtime",
                "reference a named entry from `runtimes`",
            )
        })?;
        let runtime = config.runtimes.get(runtime_name).ok_or_else(|| {
            ConfigError::validation(
                file,
                format!("services.{name}.runtime"),
                format!("references unknown runtime '{runtime_name}'"),
                "define the runtime under `runtimes`, or fix the reference",
            )
        })?;

        match runtime {
            RuntimeConfig::Process {} => {
                if service.target.is_some() {
                    return Err(ConfigError::validation(
                        file,
                        format!("services.{name}.target"),
                        "process service cannot have a runtime target",
                        "remove `target`; process services use `command`",
                    ));
                }
            }
            RuntimeConfig::Compose { .. } => {
                let target = service.target.as_deref().map(str::trim).unwrap_or_default();
                if target.is_empty() {
                    return Err(ConfigError::validation(
                        file,
                        format!("services.{name}.target"),
                        "compose service has no Compose service target",
                        "set `target:` to the service name declared in Compose",
                    ));
                }
                if !compose_targets.insert((runtime_name.to_string(), target.to_string())) {
                    return Err(ConfigError::validation(
                        file,
                        format!("services.{name}.target"),
                        format!(
                            "Compose target '{target}' is already mapped in runtime '{runtime_name}'"
                        ),
                        "map each Compose service target once per runtime",
                    ));
                }
                if service.command.is_some()
                    || service.repository.is_some()
                    || service.cwd.is_some()
                {
                    return Err(ConfigError::validation(
                        file,
                        format!("services.{name}"),
                        "compose service cannot declare command, repository, or cwd",
                        "move container execution details to the Compose file",
                    ));
                }
            }
        }

        validate_environment_sources(
            config,
            file,
            &format!("services.{name}.env_from"),
            &service.env_from,
        )?;
    }

    for (name, task) in &config.tasks {
        validate_argv(file, &format!("tasks.{name}.command"), &task.command)?;
        if let Some(check) = &task.check {
            validate_argv(file, &format!("tasks.{name}.check"), check)?;
        }
        if let Some(doctor) = &task.doctor {
            validate_argv(file, &format!("tasks.{name}.doctor"), doctor)?;
        }
        if task.timeout.is_zero() {
            return Err(ConfigError::validation(
                file,
                format!("tasks.{name}.timeout"),
                "task timeout must be greater than zero",
                "set a bounded duration such as `5m`",
            ));
        }
        let mut dependencies = HashSet::new();
        for dependency in &task.depends_on {
            if !dependencies.insert(dependency) {
                return Err(ConfigError::validation(
                    file,
                    format!("tasks.{name}.depends_on"),
                    format!("dependency '{dependency}' is listed more than once"),
                    "remove the duplicate dependency",
                ));
            }
            if dependency == name {
                return Err(ConfigError::validation(
                    file,
                    format!("tasks.{name}.depends_on"),
                    "a task cannot depend on itself",
                    "remove the self dependency",
                ));
            }
            if !unit_exists(config, dependency) {
                return Err(ConfigError::validation(
                    file,
                    format!("tasks.{name}.depends_on"),
                    format!("depends on unknown unit '{dependency}'"),
                    "define the service or task, or fix the reference",
                ));
            }
        }
        validate_environment_sources(
            config,
            file,
            &format!("tasks.{name}.env_from"),
            &task.env_from,
        )?;
    }

    Ok(())
}

fn validate_argv(file: &Path, field: &str, argv: &[String]) -> Result<(), ConfigError> {
    if argv.is_empty() || argv.iter().any(|argument| argument.is_empty()) {
        return Err(ConfigError::validation(
            file,
            field,
            "argv must contain a non-empty executable and non-empty arguments",
            "use an array such as `[./scripts/setup, --check]`",
        ));
    }
    Ok(())
}

fn validate_environment_sources(
    config: &Config,
    file: &Path,
    field: &str,
    sources: &[super::model::EnvironmentSourceConfig],
) -> Result<(), ConfigError> {
    for (index, source) in sources.iter().enumerate() {
        let provider = config
            .environment_providers
            .get(&source.provider)
            .ok_or_else(|| {
                ConfigError::validation(
                    file,
                    format!("{field}.{index}.provider"),
                    format!(
                        "references unknown environment provider '{}'",
                        source.provider
                    ),
                    "define the provider under `environment_providers`, or fix the reference",
                )
            })?;
        if let Some(reference) = &source.reference {
            if reference.trim().is_empty() {
                return Err(ConfigError::validation(
                    file,
                    format!("{field}.{index}.reference"),
                    "environment source reference cannot be empty",
                    "set the provider-specific item reference, or omit the field",
                ));
            }
        }
        for (path_field, path) in [("schema", &source.schema), ("cache", &source.cache)] {
            if let Some(path) = path {
                if path.is_absolute()
                    || path
                        .components()
                        .any(|component| matches!(component, Component::ParentDir))
                {
                    return Err(ConfigError::validation(
                        file,
                        format!("{field}.{index}.{path_field}"),
                        "environment source paths must stay inside the project",
                        "use a relative path without `..` components",
                    ));
                }
            }
        }
        match provider {
            EnvironmentProviderConfig::OnePassword { .. }
                if !source
                    .reference
                    .as_deref()
                    .is_some_and(|reference| reference.starts_with("op://")) =>
            {
                return Err(ConfigError::validation(
                    file,
                    format!("{field}.{index}.reference"),
                    "1Password reference must start with op://",
                    "use an item or field reference accepted by the 1Password CLI",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_environment_providers(config: &Config, file: &Path) -> Result<(), ConfigError> {
    for (name, provider) in &config.environment_providers {
        if let EnvironmentProviderConfig::Exec { command } = provider {
            validate_argv(
                file,
                &format!("environment_providers.{name}.command"),
                command,
            )?;
        }
    }
    Ok(())
}

fn unit_exists(config: &Config, name: &str) -> bool {
    config.services.contains_key(name) || config.tasks.contains_key(name)
}

fn is_compose_project_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        })
}

pub fn is_safe_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

fn validate_url(file: &Path, field: String, url: &str) -> Result<(), ConfigError> {
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        ConfigError::validation(
            file,
            &field,
            format!("invalid URL: {error}"),
            "use an absolute http:// or https:// URL",
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ConfigError::validation(
            file,
            field,
            format!("unsupported URL scheme '{}'", parsed.scheme()),
            "use an http:// or https:// URL",
        ));
    }
    Ok(())
}

fn validate_healthcheck(
    file: &Path,
    service: &str,
    healthcheck: &HealthcheckConfig,
) -> Result<(), ConfigError> {
    let field = format!("services.{service}.healthcheck");
    match healthcheck {
        HealthcheckConfig::Http {
            url,
            timeout,
            interval,
            retries,
            expected_status,
        } => {
            validate_url(file, format!("{field}.url"), url)?;
            validate_probe_timing(file, &field, *timeout, *interval, *retries)?;
            if expected_status.is_empty()
                || expected_status
                    .iter()
                    .any(|status| !(100..=599).contains(status))
            {
                return Err(ConfigError::validation(
                    file,
                    format!("{field}.expected_status"),
                    "expected_status must contain valid HTTP status codes (100-599)",
                    "add at least one valid status code, for example 200",
                ));
            }
        }
        HealthcheckConfig::Tcp {
            host,
            port,
            timeout,
            interval,
            retries,
        } => {
            if host.trim().is_empty() {
                return Err(ConfigError::validation(
                    file,
                    format!("{field}.host"),
                    "TCP healthcheck host cannot be empty",
                    "set a host such as 127.0.0.1",
                ));
            }
            if *port == 0 {
                return Err(ConfigError::validation(
                    file,
                    format!("{field}.port"),
                    "TCP healthcheck port must be between 1 and 65535",
                    "set the port checked by the probe",
                ));
            }
            validate_probe_timing(file, &field, *timeout, *interval, *retries)?;
        }
    }
    Ok(())
}

fn validate_probe_timing(
    file: &Path,
    field: &str,
    timeout: std::time::Duration,
    interval: std::time::Duration,
    retries: u32,
) -> Result<(), ConfigError> {
    if timeout.is_zero() || interval.is_zero() || retries == 0 {
        return Err(ConfigError::validation(
            file,
            field,
            "healthcheck timeout, interval, and retries must be greater than zero",
            "configure non-zero probe timing and at least one retry",
        ));
    }
    Ok(())
}

/// RF-05: dependencies must form a DAG. Depth-first search with a
/// recursion stack to find back-edges (cycles).
fn detect_cycles(config: &Config, file: &Path) -> Result<(), ConfigError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }

    fn visit<'a>(
        name: &'a str,
        config: &'a Config,
        marks: &mut std::collections::HashMap<&'a str, Mark>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        match marks.get(name) {
            Some(Mark::Done) => return None,
            Some(Mark::Visiting) => {
                let start = stack.iter().position(|s| *s == name).unwrap_or(0);
                let mut cycle: Vec<String> = stack[start..].iter().map(|s| s.to_string()).collect();
                cycle.push(name.to_string());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(name, Mark::Visiting);
        stack.push(name);
        let dependencies = config
            .services
            .get(name)
            .map(|service| service.depends_on.as_slice())
            .or_else(|| {
                config
                    .tasks
                    .get(name)
                    .map(|task| task.depends_on.as_slice())
            })
            .unwrap_or_default();
        for dependency in dependencies {
            if let Some(cycle) = visit(dependency.as_str(), config, marks, stack) {
                return Some(cycle);
            }
        }
        stack.pop();
        marks.insert(name, Mark::Done);
        None
    }

    let mut marks = std::collections::HashMap::new();
    let mut visited_roots: HashSet<&str> = HashSet::new();
    for name in config.services.keys().chain(config.tasks.keys()) {
        if visited_roots.contains(name.as_str()) {
            continue;
        }
        let mut stack = Vec::new();
        if let Some(cycle) = visit(name.as_str(), config, &mut marks, &mut stack) {
            return Err(ConfigError::validation(
                file,
                "services.*.depends_on",
                format!("circular dependency detected: {}", cycle.join(" → ")),
                "break the cycle by removing one of the depends_on edges above",
            ));
        }
        visited_roots.insert(name.as_str());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{
        EnvironmentProviderConfig, EnvironmentSourceConfig, EnvironmentSourceFormat, RuntimeConfig,
        ServiceConfig, TemplateConfig,
    };

    fn valid_config() -> Config {
        Config {
            version: 2,
            project: Some("demo".to_string()),
            services: HashMap::from([(
                "api".to_string(),
                ServiceConfig {
                    command: Some("cargo run".to_string()),
                    ..ServiceConfig::default()
                },
            )]),
            templates: HashMap::from([(
                "all".to_string(),
                TemplateConfig {
                    services: vec!["api".to_string()],
                },
            )]),
            ..Config::default()
        }
    }

    fn valid_v3_config() -> Config {
        Config {
            version: 3,
            project: Some("demo".to_string()),
            runtimes: HashMap::from([
                ("local".to_string(), RuntimeConfig::Process {}),
                (
                    "infra".to_string(),
                    RuntimeConfig::Compose {
                        project_name: "demo_local".to_string(),
                        files: vec!["compose.yaml".into()],
                        reconcile: false,
                        generated_files: Vec::new(),
                        profiles: vec!["development".to_string()],
                        env_file: None,
                    },
                ),
            ]),
            environment_providers: HashMap::from([(
                "company".to_string(),
                EnvironmentProviderConfig::OnePassword { account: None },
            )]),
            services: HashMap::from([
                (
                    "api".to_string(),
                    ServiceConfig {
                        runtime: Some("local".to_string()),
                        command: Some("cargo run".to_string()),
                        ..ServiceConfig::default()
                    },
                ),
                (
                    "database".to_string(),
                    ServiceConfig {
                        runtime: Some("infra".to_string()),
                        target: Some("postgres".to_string()),
                        env_from: vec![EnvironmentSourceConfig {
                            provider: "company".to_string(),
                            reference: Some("op://Development/database/environment".to_string()),
                            format: EnvironmentSourceFormat::Dotenv,
                            optional: true,
                            schema: Some("config/database.env.example".into()),
                            cache: Some(".hum/cache/database.env".into()),
                            args: Vec::new(),
                        }],
                        ..ServiceConfig::default()
                    },
                ),
            ]),
            templates: HashMap::from([(
                "all".to_string(),
                TemplateConfig {
                    services: vec!["api".to_string(), "database".to_string()],
                },
            )]),
            ..Config::default()
        }
    }

    #[test]
    fn rejects_empty_command() {
        let mut config = valid_config();
        config.services.get_mut("api").unwrap().command = Some("  ".to_string());
        assert!(validate(&config, Path::new("hum.yaml")).is_err());
    }

    #[test]
    fn rejects_invalid_log_limits_and_redaction_patterns() {
        let file = Path::new("hum.yaml");
        let mut config = valid_config();
        config.logs.max_file_bytes = 0;
        assert!(validate(&config, file).is_err());

        config.logs.max_file_bytes = 1024;
        config.logs.redact_patterns = vec!["(".to_string()];
        assert!(validate(&config, file).is_err());
    }

    #[test]
    fn rejects_unbounded_log_export_configuration() {
        let file = Path::new("hum.yaml");
        let mut config = valid_config();
        config.logs.redact_patterns = vec!["x".to_string(); MAX_REDACT_PATTERNS + 1];
        assert!(validate(&config, file).is_err());

        config.logs.redact_patterns = vec!["x".repeat(MAX_REDACT_PATTERN_BYTES + 1)];
        assert!(validate(&config, file).is_err());

        config.logs.redact_patterns.clear();
        config.logs.exporters = vec![
            crate::config::LogExporterConfig::Http {
                endpoint: "https://collector.example/events".to_string(),
                timeout: std::time::Duration::from_millis(750),
                headers: HashMap::new(),
            };
            MAX_LOG_EXPORTERS + 1
        ];
        assert!(validate(&config, file).is_err());
    }

    #[test]
    fn validates_best_effort_http_log_exporters() {
        let file = Path::new("hum.yaml");
        let mut config = valid_config();
        config.logs.exporters = vec![crate::config::LogExporterConfig::Http {
            endpoint: "http://127.0.0.1:8687/events".to_string(),
            timeout: std::time::Duration::from_millis(750),
            headers: HashMap::from([(
                "Authorization".to_string(),
                "Basic machine-local-token".to_string(),
            )]),
        }];
        assert!(validate(&config, file).is_ok());

        config.logs.exporters = vec![crate::config::LogExporterConfig::Http {
            endpoint: "http://127.0.0.1:8687/events".to_string(),
            timeout: std::time::Duration::from_millis(750),
            headers: HashMap::from([("bad header".to_string(), "value".to_string())]),
        }];
        assert!(validate(&config, file).is_err());

        config.logs.exporters = vec![crate::config::LogExporterConfig::Http {
            endpoint: "http://[::1]:8687/events".to_string(),
            timeout: std::time::Duration::from_millis(750),
            headers: HashMap::new(),
        }];
        assert!(validate(&config, file).is_ok());

        config.logs.exporters = vec![crate::config::LogExporterConfig::Http {
            endpoint: "http://collector.localhost.:8687/events".to_string(),
            timeout: std::time::Duration::from_millis(750),
            headers: HashMap::new(),
        }];
        assert!(validate(&config, file).is_ok());

        config.logs.exporters = vec![crate::config::LogExporterConfig::Http {
            endpoint: "http://[::ffff:127.0.0.1]:8687/events".to_string(),
            timeout: std::time::Duration::from_millis(750),
            headers: HashMap::new(),
        }];
        assert!(validate(&config, file).is_ok());

        config.logs.exporters = vec![crate::config::LogExporterConfig::Http {
            endpoint: "file:///tmp/logs".to_string(),
            timeout: std::time::Duration::from_millis(750),
            headers: HashMap::new(),
        }];
        assert!(validate(&config, file).is_err());

        config.logs.exporters = vec![crate::config::LogExporterConfig::Http {
            endpoint: "http://user:secret@127.0.0.1:8687/events".to_string(),
            timeout: std::time::Duration::from_millis(750),
            headers: HashMap::new(),
        }];
        assert!(validate(&config, file).is_err());

        config.logs.exporters = vec![crate::config::LogExporterConfig::Http {
            endpoint: "http://127.0.0.1:8687/events?token=secret".to_string(),
            timeout: std::time::Duration::from_millis(750),
            headers: HashMap::new(),
        }];
        assert!(validate(&config, file).is_err());

        config.logs.exporters = vec![crate::config::LogExporterConfig::Http {
            endpoint: "http://collector.example/events".to_string(),
            timeout: std::time::Duration::from_millis(750),
            headers: HashMap::new(),
        }];
        assert!(validate(&config, file).is_err());

        config.logs.exporters = vec![crate::config::LogExporterConfig::Http {
            endpoint: "https://collector.example/events".to_string(),
            timeout: std::time::Duration::from_millis(750),
            headers: HashMap::new(),
        }];
        assert!(validate(&config, file).is_ok());
    }

    #[test]
    fn rejects_zero_port_and_invalid_url() {
        let mut config = valid_config();
        config.services.get_mut("api").unwrap().port = Some(0);
        assert!(validate(&config, Path::new("hum.yaml")).is_err());

        let mut config = valid_config();
        config.services.get_mut("api").unwrap().url = Some("localhost:3000".to_string());
        assert!(validate(&config, Path::new("hum.yaml")).is_err());
    }

    #[test]
    fn rejects_port_and_name_collisions() {
        let mut config = valid_config();
        config.services.get_mut("api").unwrap().port = Some(3000);
        config.services.insert(
            "worker".to_string(),
            ServiceConfig {
                command: Some("cargo run".to_string()),
                port: Some(3000),
                ..ServiceConfig::default()
            },
        );
        assert!(validate(&config, Path::new("hum.yaml")).is_err());

        let mut config = valid_config();
        config.services.insert(
            "API".to_string(),
            ServiceConfig {
                command: Some("cargo run".to_string()),
                ..ServiceConfig::default()
            },
        );
        assert!(validate(&config, Path::new("hum.yaml")).is_err());
    }

    #[test]
    fn readiness_requires_its_underlying_signal() {
        let mut config = valid_config();
        config.services.get_mut("api").unwrap().depends_on_ready =
            Some(crate::config::ReadyMode::Listening);
        assert!(validate(&config, Path::new("hum.yaml")).is_err());

        let mut config = valid_config();
        config.services.get_mut("api").unwrap().depends_on_ready =
            Some(crate::config::ReadyMode::Healthy);
        assert!(validate(&config, Path::new("hum.yaml")).is_err());
    }

    #[test]
    fn accepts_v3_process_compose_and_one_password_contract() {
        validate(&valid_v3_config(), Path::new("hum.yaml")).unwrap();
    }

    #[test]
    fn rejects_v3_fields_in_legacy_configs() {
        let mut config = valid_config();
        config
            .runtimes
            .insert("local".to_string(), RuntimeConfig::Process {});
        let error = validate(&config, Path::new("hum.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("require configuration version 3"), "{error}");

        let mut config = valid_config();
        config
            .services
            .get_mut("api")
            .unwrap()
            .env_overrides
            .insert("API_URL".to_string(), "http://localhost".to_string());
        let error = validate(&config, Path::new("hum.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("env_overrides"), "{error}");
    }

    #[test]
    fn rejects_invalid_compose_and_provider_references() {
        let mut config = valid_v3_config();
        let RuntimeConfig::Compose { project_name, .. } = config.runtimes.get_mut("infra").unwrap()
        else {
            unreachable!();
        };
        *project_name = "Demo Local".to_string();
        assert!(validate(&config, Path::new("hum.yaml")).is_err());

        let mut config = valid_v3_config();
        config.services.get_mut("database").unwrap().env_from[0].provider = "missing".to_string();
        let error = validate(&config, Path::new("hum.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown environment provider"), "{error}");

        let mut config = valid_v3_config();
        config.services.get_mut("database").unwrap().env_from[0].reference =
            Some("Development/database/environment".to_string());
        let error = validate(&config, Path::new("hum.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("must start with op://"), "{error}");
    }

    #[test]
    fn accepts_exec_provider_and_rejects_empty_command() {
        let mut config = valid_v3_config();
        config.environment_providers.insert(
            "shell".to_string(),
            EnvironmentProviderConfig::Exec {
                command: vec!["compri".to_string(), "env".to_string()],
            },
        );
        validate(&config, Path::new("hum.yaml")).unwrap();

        config.environment_providers.insert(
            "shell".to_string(),
            EnvironmentProviderConfig::Exec { command: vec![] },
        );
        let error = validate(&config, Path::new("hum.yaml"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("argv must contain a non-empty executable"),
            "{error}"
        );
    }

    #[test]
    fn exec_provider_sources_do_not_require_a_reference() {
        let mut config = valid_v3_config();
        config.environment_providers.insert(
            "shell".to_string(),
            EnvironmentProviderConfig::Exec {
                command: vec!["compri".to_string(), "env".to_string()],
            },
        );
        config.services.get_mut("database").unwrap().env_from = vec![EnvironmentSourceConfig {
            provider: "shell".to_string(),
            reference: None,
            format: EnvironmentSourceFormat::Dotenv,
            optional: true,
            schema: Some("config/database.env.example".into()),
            cache: Some(".hum/cache/database.env".into()),
            args: vec!["compri-applications".to_string()],
        }];
        validate(&config, Path::new("hum.yaml")).unwrap();
    }
}
