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
    #[serde(default)]
    pub services: HashMap<String, ServiceConfig>,
    #[serde(default, alias = "profiles")]
    pub templates: HashMap<String, TemplateConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub repository: Option<String>,
    pub cwd: Option<PathBuf>,
    pub command: Option<String>,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub env_file: Option<PathBuf>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub healthcheck: Option<HealthcheckConfig>,
    #[serde(default)]
    pub requires: RequirementsConfig,
    /// started | healthy — when a dependent service is allowed to start.
    #[serde(default)]
    pub depends_on_ready: Option<ReadyMode>,
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
