//! Layer merge order and redaction-safe per-field provenance.

use std::{collections::BTreeMap, fs, path::Path};

use imagegen_bridge_core::{BridgeError, ErrorCode};
use serde::de::IntoDeserializer as _;
use serde_json::{Map, Value};

use crate::BridgeConfig;

const DEFAULT_ENV_PREFIX: &str = "IMAGEGEN_BRIDGE__";
const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_OVERRIDE_BYTES: usize = 1024 * 1024;

/// Layer category that supplied an effective field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// Built-in safe default.
    Default,
    /// TOML configuration file.
    File,
    /// Environment variable.
    Environment,
    /// Explicit command-line or embedding override.
    Override,
}

/// Redaction-safe origin of one effective configuration leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOrigin {
    /// Source layer.
    pub source: ConfigSource,
    /// Field path or environment variable name, never its value.
    pub key: String,
}

/// One explicit set or unset operation applied after environment variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOverride {
    /// Dotted configuration field path.
    pub key: String,
    /// TOML scalar/array value; `None` clears an optional field.
    pub value: Option<String>,
}

impl ConfigOverride {
    /// Creates a highest-precedence set operation.
    #[must_use]
    pub fn set(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: Some(value.into()),
        }
    }

    /// Creates a highest-precedence optional-field clear operation.
    #[must_use]
    pub fn unset(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: None,
        }
    }
}

/// Typed effective configuration plus redaction-safe provenance.
#[derive(Clone)]
pub struct ResolvedConfig {
    /// Fully merged typed configuration.
    pub config: BridgeConfig,
    provenance: BTreeMap<String, ConfigOrigin>,
}

impl std::fmt::Debug for ResolvedConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedConfig")
            .field("version", &self.config.version)
            .field("default_provider", &self.config.default_provider)
            .field("provenance_fields", &self.provenance.len())
            .finish_non_exhaustive()
    }
}

impl ResolvedConfig {
    /// Returns the source of one dotted effective field without returning its value.
    #[must_use]
    pub fn origin(&self, field: &str) -> Option<&ConfigOrigin> {
        self.provenance.get(field)
    }

    /// Returns all effective field origins in stable path order.
    #[must_use]
    pub const fn provenance(&self) -> &BTreeMap<String, ConfigOrigin> {
        &self.provenance
    }
}

/// Deterministic `defaults < file < environment < overrides` loader.
#[derive(Debug, Clone)]
pub struct ConfigLoader {
    env_prefix: String,
    max_file_bytes: u64,
}

impl Default for ConfigLoader {
    fn default() -> Self {
        Self {
            env_prefix: DEFAULT_ENV_PREFIX.to_owned(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

impl ConfigLoader {
    /// Creates a loader with a validated environment prefix and file bound.
    pub fn new(env_prefix: impl Into<String>, max_file_bytes: u64) -> Result<Self, BridgeError> {
        let env_prefix = env_prefix.into();
        if env_prefix.is_empty()
            || !env_prefix
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            || max_file_bytes == 0
        {
            return Err(config_error("configuration loader limits are invalid"));
        }
        Ok(Self {
            env_prefix,
            max_file_bytes,
        })
    }

    /// Resolves using the current process environment.
    pub fn resolve(
        &self,
        file: Option<&Path>,
        overrides: &[ConfigOverride],
    ) -> Result<ResolvedConfig, BridgeError> {
        let environment = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)));
        self.resolve_with_environment(file, environment, overrides)
    }

    /// Resolves with an explicit environment iterator for deterministic embedding/tests.
    pub fn resolve_with_environment(
        &self,
        file: Option<&Path>,
        environment: impl IntoIterator<Item = (String, String)>,
        overrides: &[ConfigOverride],
    ) -> Result<ResolvedConfig, BridgeError> {
        let mut merged = serde_json::to_value(BridgeConfig::default())
            .map_err(|_| config_error("could not encode default configuration"))?;
        let mut provenance = BTreeMap::new();
        record_leaves(&merged, "", ConfigSource::Default, None, &mut provenance);

        if let Some(path) = file {
            let layer = self.read_file(path)?;
            merge_values(
                &mut merged,
                layer,
                "",
                ConfigSource::File,
                None,
                &mut provenance,
            );
        }

        let mut environment: Vec<_> = environment
            .into_iter()
            .filter(|(key, _)| key.starts_with(&self.env_prefix))
            .collect();
        environment.sort_by(|left, right| left.0.cmp(&right.0));
        for (variable, raw) in environment {
            let key = variable
                .strip_prefix(&self.env_prefix)
                .ok_or_else(|| config_error("environment prefix handling failed"))?;
            let segments = environment_segments(key)?;
            let value = parse_override_value(&raw)?;
            set_path(
                &mut merged,
                &segments,
                value,
                ConfigSource::Environment,
                &variable,
                &mut provenance,
            )?;
        }

        for operation in overrides {
            let segments = dotted_segments(&operation.key)?;
            let value = operation
                .value
                .as_deref()
                .map(parse_override_value)
                .transpose()?
                .unwrap_or(Value::Null);
            set_path(
                &mut merged,
                &segments,
                value,
                ConfigSource::Override,
                &operation.key,
                &mut provenance,
            )?;
        }

        let deserializer = merged.into_deserializer();
        let config: BridgeConfig =
            serde_path_to_error::deserialize(deserializer).map_err(|error| {
                config_error("effective configuration does not match the schema")
                    .with_detail("field", error.path().to_string())
            })?;
        Ok(ResolvedConfig { config, provenance })
    }

    fn read_file(&self, path: &Path) -> Result<Value, BridgeError> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| config_error("could not inspect configuration file"))?;
        if !metadata.file_type().is_file() || metadata.len() > self.max_file_bytes {
            return Err(config_error(
                "configuration file must be a bounded regular file",
            ));
        }
        let bytes =
            fs::read(path).map_err(|_| config_error("could not read configuration file"))?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| config_error("configuration file is not valid UTF-8"))?;
        let toml: toml::Value = toml::from_str(text)
            .map_err(|_| config_error("configuration file is not valid TOML"))?;
        serde_json::to_value(toml)
            .map_err(|_| config_error("could not normalize configuration file"))
    }
}

fn parse_override_value(raw: &str) -> Result<Value, BridgeError> {
    if raw.len() > MAX_OVERRIDE_BYTES {
        return Err(config_error(
            "configuration override exceeds the size limit",
        ));
    }
    if raw.is_empty() {
        return Ok(Value::String(String::new()));
    }
    let wrapped = format!("value = {raw}");
    if let Ok(document) = toml::from_str::<toml::Table>(&wrapped)
        && let Some(value) = document.get("value")
    {
        return serde_json::to_value(value)
            .map_err(|_| config_error("could not normalize configuration override"));
    }
    Ok(Value::String(raw.to_owned()))
}

fn environment_segments(key: &str) -> Result<Vec<String>, BridgeError> {
    if key.is_empty() {
        return Err(config_error("configuration environment key is empty"));
    }
    let mut segments = Vec::new();
    for segment in key.split("__") {
        if segment.is_empty()
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(config_error("configuration environment key is invalid"));
        }
        segments.push(segment.to_ascii_lowercase());
    }
    Ok(segments)
}

fn dotted_segments(key: &str) -> Result<Vec<String>, BridgeError> {
    if key.is_empty() {
        return Err(config_error("configuration override key is empty"));
    }
    let mut segments = Vec::new();
    for segment in key.split('.') {
        if segment.is_empty()
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(config_error("configuration override key is invalid"));
        }
        segments.push(segment.to_owned());
    }
    Ok(segments)
}

fn set_path(
    root: &mut Value,
    segments: &[String],
    value: Value,
    source: ConfigSource,
    source_key: &str,
    provenance: &mut BTreeMap<String, ConfigOrigin>,
) -> Result<(), BridgeError> {
    let (last, parents) = segments
        .split_last()
        .ok_or_else(|| config_error("configuration field path is empty"))?;
    let mut current = root;
    for segment in parents {
        let object = current
            .as_object_mut()
            .ok_or_else(|| config_error("configuration field path crosses a scalar value"))?;
        current = object
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let object = current
        .as_object_mut()
        .ok_or_else(|| config_error("configuration field parent is not an object"))?;
    let path = segments.join(".");
    record_leaves(&value, &path, source, Some(source_key), provenance);
    object.insert(last.clone(), value);
    Ok(())
}

fn merge_values(
    destination: &mut Value,
    source_value: Value,
    path: &str,
    source: ConfigSource,
    source_key: Option<&str>,
    provenance: &mut BTreeMap<String, ConfigOrigin>,
) {
    match source_value {
        Value::Object(source_object) => {
            if let Some(destination_object) = destination.as_object_mut() {
                for (key, value) in source_object {
                    let child_path = join_path(path, &key);
                    if let Some(existing) = destination_object.get_mut(&key)
                        && existing.is_object()
                        && value.is_object()
                    {
                        merge_values(existing, value, &child_path, source, source_key, provenance);
                    } else {
                        record_leaves(&value, &child_path, source, source_key, provenance);
                        destination_object.insert(key, value);
                    }
                }
            } else {
                *destination = Value::Object(source_object);
                record_leaves(destination, path, source, source_key, provenance);
            }
        }
        value => {
            *destination = value;
            record_leaves(destination, path, source, source_key, provenance);
        }
    }
}

fn record_leaves(
    value: &Value,
    path: &str,
    source: ConfigSource,
    source_key: Option<&str>,
    provenance: &mut BTreeMap<String, ConfigOrigin>,
) {
    if let Value::Object(object) = value {
        for (key, child) in object {
            record_leaves(child, &join_path(path, key), source, source_key, provenance);
        }
    } else if !path.is_empty() {
        provenance.insert(
            path.to_owned(),
            ConfigOrigin {
                source,
                key: source_key.unwrap_or(path).to_owned(),
            },
        );
    }
}

fn join_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_owned()
    } else {
        format!("{parent}.{child}")
    }
}

fn config_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Configuration, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;

    use super::*;

    #[test]
    fn merges_defaults_file_environment_and_overrides_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("bridge.toml");
        fs::write(
            &file,
            r#"
[runtime]
default_timeout_ms = 1000

[runtime.global]
max_concurrent = 2

[artifacts]
public_base_url = "https://images.example.test/v1/"
"#,
        )
        .unwrap();
        let resolved = ConfigLoader::default()
            .resolve_with_environment(
                Some(&file),
                [
                    (
                        "IMAGEGEN_BRIDGE__RUNTIME__DEFAULT_TIMEOUT_MS".to_owned(),
                        "2000".to_owned(),
                    ),
                    (
                        "IMAGEGEN_BRIDGE__INPUTS__REMOTE__ALLOWED_PORTS".to_owned(),
                        "[8080, 8443]".to_owned(),
                    ),
                ],
                &[
                    ConfigOverride::set("runtime.default_timeout_ms", "3000"),
                    ConfigOverride::unset("artifacts.public_base_url"),
                ],
            )
            .unwrap();
        assert_eq!(resolved.config.runtime.default_timeout_ms, 3000);
        assert_eq!(resolved.config.runtime.global.max_concurrent, 2);
        assert_eq!(resolved.config.inputs.remote.allowed_ports, [8080, 8443]);
        assert_eq!(resolved.config.artifacts.public_base_url, None);
        assert!(!resolved.config.server.metrics.enabled);
        assert!(resolved.config.server.tracing.enabled);
        assert_eq!(
            resolved
                .origin("runtime.default_timeout_ms")
                .unwrap()
                .source,
            ConfigSource::Override
        );
        assert_eq!(
            resolved
                .origin("runtime.global.max_concurrent")
                .unwrap()
                .source,
            ConfigSource::File
        );
        assert_eq!(
            resolved.origin("inputs.remote.allowed_ports").unwrap().key,
            "IMAGEGEN_BRIDGE__INPUTS__REMOTE__ALLOWED_PORTS"
        );
    }

    #[test]
    fn rejects_unknown_fields_without_reflecting_their_values() {
        let secret = "private-value-that-must-not-be-reflected";
        let error = ConfigLoader::default()
            .resolve_with_environment(
                None,
                [(
                    "IMAGEGEN_BRIDGE__UNKNOWN_SECRET".to_owned(),
                    secret.to_owned(),
                )],
                &[],
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Configuration);
        assert!(!error.message.contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }

    #[test]
    fn rejects_oversized_or_non_regular_files() {
        let directory = tempfile::tempdir().unwrap();
        let loader = ConfigLoader::new("TEST__", 4).unwrap();
        let file = directory.path().join("large.toml");
        fs::write(&file, b"version = 1").unwrap();
        assert!(loader.resolve(Some(&file), &[]).is_err());
        assert!(loader.resolve(Some(directory.path()), &[]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_configuration_files() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.toml");
        let link = directory.path().join("link.toml");
        fs::write(&target, b"version = 1").unwrap();
        symlink(&target, &link).unwrap();
        assert!(ConfigLoader::default().resolve(Some(&link), &[]).is_err());
    }
}
