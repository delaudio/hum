use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    /// Stable project identifier required by the v2 project/template model.
    /// It remains optional while loading v1 files for migration diagnostics.
    pub project: Option<String>,
    #[serde(default)]
    pub repositories: HashMap<String, RepositoryConfig>,
    /// Named runtime adapters. Version 1/2 configurations implicitly use the
    /// built-in process runtime; version 3 services select one explicitly.
    #[serde(default)]
    pub runtimes: HashMap<String, RuntimeConfig>,
    /// Named environment providers. Providers describe how values are
    /// obtained; service declarations only refer to a provider by name.
    #[serde(default)]
    pub environment_providers: HashMap<String, EnvironmentProviderConfig>,
    #[serde(default)]
    pub services: HashMap<String, ServiceConfig>,
    #[serde(default)]
    pub tasks: HashMap<String, TaskConfig>,
    #[serde(default, alias = "profiles")]
    pub templates: HashMap<String, TemplateConfig>,
    #[serde(default)]
    pub logs: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    #[serde(default = "default_log_file_bytes")]
    pub max_file_bytes: u64,
    #[serde(default = "default_rotated_files")]
    pub rotated_files: usize,
    #[serde(default = "default_log_line_bytes")]
    pub max_line_bytes: usize,
    #[serde(default, with = "humantime_serde::option")]
    pub retention: Option<Duration>,
    #[serde(default)]
    pub redact_patterns: Vec<String>,
    /// Optional best-effort copies of process-runtime logs. Persistent Hum
    /// logs remain authoritative and exporters never gate service execution.
    #[serde(default)]
    pub exporters: Vec<LogExporterConfig>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: default_log_file_bytes(),
            rotated_files: default_rotated_files(),
            max_line_bytes: default_log_line_bytes(),
            retention: None,
            redact_patterns: Vec::new(),
            exporters: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LogExporterConfig {
    Http {
        endpoint: String,
        #[serde(default = "default_log_export_timeout", with = "humantime_serde")]
        timeout: Duration,
        /// Static request headers. Values commonly contain machine-local
        /// credentials, so callers should keep them in untracked overrides.
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

fn default_log_export_timeout() -> Duration {
    Duration::from_millis(750)
}

fn default_log_file_bytes() -> u64 {
    10 * 1024 * 1024
}

fn default_rotated_files() -> usize {
    3
}

fn default_log_line_bytes() -> usize {
    64 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    /// Version 3 runtime name. Omitted by legacy process-only configurations.
    pub runtime: Option<String>,
    /// Runtime-native service identifier, for example a Compose service name.
    pub target: Option<String>,
    pub repository: Option<String>,
    pub cwd: Option<PathBuf>,
    pub command: Option<String>,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub env_file: Option<PathBuf>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Values applied after provider-backed dotenv sources. This is useful for
    /// machine-local runtime overlays that must project container endpoints to
    /// host endpoints without copying or editing provider data. Inherited
    /// process values and explicit CLI `--env` values still take precedence.
    /// Requires configuration version 3.
    #[serde(default)]
    pub env_overrides: HashMap<String, String>,
    #[serde(default)]
    pub env_from: Vec<EnvironmentSourceConfig>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub healthcheck: Option<HealthcheckConfig>,
    #[serde(default)]
    pub requires: RequirementsConfig,
    /// started | healthy — when a dependent service is allowed to start.
    #[serde(default)]
    pub depends_on_ready: Option<ReadyMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskConfig {
    /// Direct argv. The first element is the executable; hum never inserts a
    /// shell around project tasks.
    pub command: Vec<String>,
    pub check: Option<Vec<String>>,
    /// Optional read-only direct argv executed only by `hum doctor`. Provider
    /// values are deliberately unavailable to this diagnostic command.
    pub doctor: Option<Vec<String>>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub env_from: Vec<EnvironmentSourceConfig>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "default_task_timeout", with = "humantime_serde")]
    pub timeout: Duration,
}

fn default_task_timeout() -> Duration {
    Duration::from_secs(300)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RuntimeConfig {
    Process {},
    Compose {
        project_name: String,
        files: Vec<PathBuf>,
        /// Reapply selected running targets with `compose up`. Disabled by
        /// default; product packs opt in when generated layers or provider
        /// environments must converge on every start.
        #[serde(default)]
        reconcile: bool,
        /// Runtime layers created by project tasks. Existing files are passed
        /// to Compose; absent files are valid before their producer runs.
        #[serde(default)]
        generated_files: Vec<PathBuf>,
        #[serde(default)]
        profiles: Vec<String>,
        env_file: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum EnvironmentProviderConfig {
    OnePassword { account: Option<String> },
    Exec { command: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSourceConfig {
    pub provider: String,
    pub reference: Option<String>,
    #[serde(default)]
    pub format: EnvironmentSourceFormat,
    #[serde(default)]
    pub optional: bool,
    /// Optional key contract for dotenv-shaped provider results.
    pub schema: Option<PathBuf>,
    /// Optional owner-only plaintext cache used when the provider is offline.
    pub cache: Option<PathBuf>,
    /// Extra argv appended after an `Exec` provider's base command, letting a
    /// single provider be parameterized per `env_from` entry.
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvironmentSourceFormat {
    #[default]
    Dotenv,
    /// A flat JSON object of string values. Unlike `Dotenv`, keys are not
    /// restricted to identifier syntax (e.g. keys containing `/` or `:`),
    /// which some providers need to emit (npm registry auth config keys,
    /// for instance).
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadyMode {
    Started,
    Listening,
    Healthy,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RequirementsConfig {
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub files: Vec<PathBuf>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum HealthcheckConfig {
    Http {
        url: String,
        #[serde(default = "default_timeout", with = "humantime_serde")]
        timeout: Duration,
        #[serde(default = "default_interval", with = "humantime_serde")]
        interval: Duration,
        #[serde(default = "default_retries")]
        retries: u32,
        #[serde(default = "default_status_codes")]
        expected_status: Vec<u16>,
    },
    Tcp {
        host: String,
        port: u16,
        #[serde(default = "default_timeout", with = "humantime_serde")]
        timeout: Duration,
        #[serde(default = "default_interval", with = "humantime_serde")]
        interval: Duration,
        #[serde(default = "default_retries")]
        retries: u32,
    },
}

fn default_timeout() -> Duration {
    Duration::from_secs(1)
}

fn default_interval() -> Duration {
    Duration::from_secs(2)
}

fn default_retries() -> u32 {
    10
}

fn default_status_codes() -> Vec<u16> {
    vec![200]
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TemplateConfig {
    #[serde(default)]
    pub services: Vec<String>,
}
