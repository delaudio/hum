use std::path::PathBuf;

use thiserror::Error;

/// A configuration error with enough context to point the user at a fix.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file not found (searched from current directory upward)")]
    NotFound,

    #[error("failed to read {file}: {source}\n  → check that the file exists and is readable")]
    Io {
        file: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid YAML in {file}{location}\n  {description}\n  → {hint}")]
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
    pub fn from_yaml(file: PathBuf, err: yaml_serde::Error) -> Self {
        let location = match err.location() {
            Some(loc) => format!(" (line {}, column {})", loc.line(), loc.column()),
            None => String::new(),
        };
        ConfigError::Parse {
            file,
            location,
            description: err.to_string(),
            hint: if err.to_string().contains("unknown field") {
                "rename or remove the unknown field; valid field names are listed above".into()
            } else {
                "check indentation and YAML syntax around the reported location".into()
            },
        }
    }

    pub fn from_yaml_with_source(file: PathBuf, err: yaml_serde::Error, source: &str) -> Self {
        let description = err.to_string();
        let location = unknown_field_name(&description)
            .and_then(|field| locate_field(source, field))
            .map(|(line, column)| format!(" (line {line}, column {column})"))
            .or_else(|| {
                err.location()
                    .map(|loc| format!(" (line {}, column {})", loc.line(), loc.column()))
            })
            .unwrap_or_default();
        ConfigError::Parse {
            file,
            location,
            hint: if description.contains("unknown field") {
                "rename or remove the unknown field; valid field names are listed above".into()
            } else {
                "check indentation and YAML syntax around the reported location".into()
            },
            description,
        }
    }

    pub fn unknown_field(
        file: &std::path::Path,
        field: &str,
        valid: &[&str],
        source: &str,
    ) -> Self {
        let location = locate_field(source, field)
            .map(|(line, column)| format!(" (line {line}, column {column})"))
            .unwrap_or_default();
        ConfigError::Parse {
            file: file.to_path_buf(),
            location,
            description: format!(
                "unknown field `{field}`, expected one of {}",
                valid
                    .iter()
                    .map(|field| format!("`{field}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            hint: "rename or remove the unknown field; valid field names are listed above".into(),
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

fn unknown_field_name(description: &str) -> Option<&str> {
    description
        .split_once("unknown field `")
        .and_then(|(_, rest)| rest.split_once('`'))
        .map(|(field, _)| field)
}

fn locate_field(source: &str, field: &str) -> Option<(usize, usize)> {
    source.lines().enumerate().find_map(|(line, text)| {
        let code = text.split('#').next()?.trim_end();
        let (candidate, _) = code.split_once(':')?;
        let candidate = candidate.trim();
        let unquoted = candidate.trim_matches(|character| character == '\'' || character == '"');
        (unquoted == field).then(|| (line + 1, text.find(field).unwrap_or(0) + 1))
    })
}
