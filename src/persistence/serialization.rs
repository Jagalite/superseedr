// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform-neutral versioned persistence codecs.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io;

pub(crate) const SCHEMA_VERSION: u32 = 1;

pub(crate) fn serialize_versioned_toml<T: Serialize>(value: &T) -> io::Result<String> {
    let mut toml_value = toml::Value::try_from(value).map_err(io::Error::other)?;
    let table = toml_value
        .as_table_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Expected TOML table"))?;
    table.insert(
        "schema_version".to_string(),
        toml::Value::Integer(i64::from(SCHEMA_VERSION)),
    );
    toml::to_string_pretty(&toml_value).map_err(io::Error::other)
}

pub(crate) fn deserialize_versioned_toml<T: DeserializeOwned>(content: &str) -> io::Result<T> {
    let parsed: toml::Value = toml::from_str(content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let Some(table) = parsed.as_table() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Expected TOML table",
        ));
    };

    if let Some(schema_version_value) = table.get("schema_version") {
        let Some(schema_version) = schema_version_value.as_integer() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "schema_version must be an integer",
            ));
        };
        if schema_version != i64::from(SCHEMA_VERSION) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported schema version {schema_version}"),
            ));
        }

        let mut stripped = table.clone();
        stripped.remove("schema_version");
        return toml::Value::Table(stripped)
            .try_into()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
    }

    toml::from_str(content).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn serialize_versioned_json<T: Serialize>(value: &T) -> io::Result<String> {
    let mut json_value = serde_json::to_value(value).map_err(io::Error::other)?;
    let object = json_value
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Expected JSON object"))?;
    object.insert(
        "schema_version".to_string(),
        serde_json::Value::from(SCHEMA_VERSION),
    );
    serde_json::to_string_pretty(&json_value).map_err(io::Error::other)
}

pub(crate) fn deserialize_versioned_json<T: DeserializeOwned>(content: &str) -> io::Result<T> {
    let parsed: serde_json::Value = serde_json::from_str(content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let Some(object) = parsed.as_object() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Expected JSON object",
        ));
    };

    if let Some(schema_version_value) = object.get("schema_version") {
        let Some(schema_version) = schema_version_value.as_u64() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "schema_version must be an unsigned integer",
            ));
        };
        if schema_version != u64::from(SCHEMA_VERSION) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported schema version {schema_version}"),
            ));
        }

        let mut stripped = object.clone();
        stripped.remove("schema_version");
        return serde_json::from_value(serde_json::Value::Object(stripped))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
    }

    serde_json::from_str(content).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
