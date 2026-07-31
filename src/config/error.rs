use std::path::PathBuf;

use thiserror::Error;

/// A configuration error with enough context to point the user at a fix.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file not found (searched from current directory upward)")]
    NotFound,

    #[error(
        "failed to read {file}: {source}\n  → check that the file exists and is readable"
    )]
    Io {
        file: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "invalid YAML in {file}{location}\n  {description}\n  → {hint}"
    )]
    Parse {
        file: PathBuf,
        location: String,
        description: String,
        hint: String,
    },

    #[error("{file}: {field}: {description}\n  → {hint}")]
    Validation {
        file: PathBuf,
        field: String,
        description: String,
        hint: String,
    },
}

impl ConfigError {
    pub fn from_yaml(file: PathBuf, err: serde_yaml::Error) -> Self {
        let location = match err.location() {
            Some(loc) => format!(" (line {}, column {})", loc.line(), loc.column()),
            None => String::new(),
        };
        ConfigError::Parse {
            file,
            location,
            description: err.to_string(),
            hint: "check indentation and YAML syntax around the reported location".into(),
        }
    }

    pub fn validation(
        file: impl Into<PathBuf>,
        field: impl Into<String>,
        description: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        ConfigError::Validation {
            file: file.into(),
            field: field.into(),
            description: description.into(),
            hint: hint.into(),
        }
    }
}
