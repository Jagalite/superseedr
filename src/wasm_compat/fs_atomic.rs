// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(dead_code, unused_imports)]

//! WASM compatibility for code paths that share versioned codecs with native persistence.
//!
//! Browser effects never persist through this module. The write functions fail closed if a
//! native persistence path is accidentally reached.

pub(crate) use crate::serialization::{
    deserialize_versioned_json, deserialize_versioned_toml, serialize_versioned_json,
    serialize_versioned_toml,
};
use serde::Serialize;
use std::io;
use std::path::Path;

fn unsupported_write() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "native persistence is unavailable in the browser runtime",
    )
}

pub(crate) fn write_bytes_atomically(_path: &Path, _bytes: &[u8]) -> io::Result<()> {
    Err(unsupported_write())
}

pub(crate) fn publish_bytes_atomically(_path: &Path, _bytes: &[u8]) -> io::Result<()> {
    Err(unsupported_write())
}

pub(crate) fn write_string_atomically(_path: &Path, _content: &str) -> io::Result<()> {
    Err(unsupported_write())
}

pub(crate) fn publish_string_atomically(_path: &Path, _content: &str) -> io::Result<()> {
    Err(unsupported_write())
}

pub(crate) fn write_toml_atomically<T: Serialize>(_path: &Path, _value: &T) -> io::Result<()> {
    Err(unsupported_write())
}

pub(crate) async fn publish_bytes_atomically_async(_path: &Path, _bytes: &[u8]) -> io::Result<()> {
    Err(unsupported_write())
}
