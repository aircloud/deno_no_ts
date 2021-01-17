// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use deno_core::ModuleSpecifier;
use serde::Serialize;
use serde::Serializer;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;

// Warning! The values in this enum are duplicated in tsc/99_main_compiler.js
// Update carefully!
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum MediaType {
  JavaScript = 0,
  Json = 5,
  Wasm = 6,
  SourceMap = 8,
  Unknown = 9,
}

impl fmt::Display for MediaType {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let value = match self {
      MediaType::JavaScript => "JavaScript",
      MediaType::Json => "Json",
      MediaType::Wasm => "Wasm",
      MediaType::SourceMap => "SourceMap",
      MediaType::Unknown => "Unknown",
    };
    write!(f, "{}", value)
  }
}

impl<'a> From<&'a Path> for MediaType {
  fn from(path: &'a Path) -> Self {
    MediaType::from_path(path)
  }
}

impl<'a> From<&'a PathBuf> for MediaType {
  fn from(path: &'a PathBuf) -> Self {
    MediaType::from_path(path)
  }
}

impl<'a> From<&'a String> for MediaType {
  fn from(specifier: &'a String) -> Self {
    MediaType::from_path(&PathBuf::from(specifier))
  }
}

impl<'a> From<&'a ModuleSpecifier> for MediaType {
  fn from(specifier: &'a ModuleSpecifier) -> Self {
    let url = specifier.as_url();
    let path = if url.scheme() == "file" {
      if let Ok(path) = url.to_file_path() {
        path
      } else {
        PathBuf::from(url.path())
      }
    } else {
      PathBuf::from(url.path())
    };
    MediaType::from_path(&path)
  }
}

impl Default for MediaType {
  fn default() -> Self {
    MediaType::Unknown
  }
}

impl MediaType {
  fn from_path(path: &Path) -> Self {
    match path.extension() {
      None => match path.file_name() {
        None => MediaType::Unknown,
        Some(os_str) => match os_str.to_str() {
          _ => MediaType::Unknown,
        },
      },
      Some(os_str) => match os_str.to_str() {
        // Hint: ts or tsx actually not support in this version
        Some("js") => MediaType::JavaScript,

        Some("mjs") => MediaType::JavaScript,
        Some("cjs") => MediaType::JavaScript,
        Some("json") => MediaType::Json,
        Some("wasm") => MediaType::Wasm,

        Some("map") => MediaType::SourceMap,
        _ => MediaType::Unknown,
      },
    }
  }

  /// Convert a MediaType to a `ts.Extension`.
  ///
  /// *NOTE* This is defined in TypeScript as a string based enum.  Changes to
  /// that enum in TypeScript should be reflected here.
  pub fn as_ts_extension(&self) -> String {
    let ext = match self {
      MediaType::JavaScript => ".js",
      MediaType::Json => ".json",
      // TypeScript doesn't have an "unknown", so we will treat WASM as JS for
      // mapping purposes, though in reality, it is unlikely to ever be passed
      // to the compiler.
      MediaType::Wasm => ".js",
      // TypeScript doesn't have an "source map", so we will treat SourceMap as
      // JS for mapping purposes, though in reality, it is unlikely to ever be
      // passed to the compiler.
      MediaType::SourceMap => ".js",
      // TypeScript doesn't have an "unknown", so we will treat WASM as JS for
      // mapping purposes, though in reality, it is unlikely to ever be passed
      // to the compiler.
      MediaType::Unknown => ".js",
    };

    ext.into()
  }

  /// Map the media type to a `ts.ScriptKind`
  pub fn as_ts_script_kind(&self) -> i32 {
    match self {
      MediaType::JavaScript => 1,
      MediaType::Json => 5,
      _ => 0,
    }
  }
}

impl Serialize for MediaType {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    let value = match self {
      MediaType::JavaScript => 0_i32,
      MediaType::Json => 5_i32,
      MediaType::Wasm => 6_i32,
      MediaType::SourceMap => 8_i32,
      MediaType::Unknown => 9_i32,
    };
    Serialize::serialize(&value, serializer)
  }
}

/// Serialize a `MediaType` enum into a human readable string.  The default
/// serialization for media types is and integer.
///
/// TODO(@kitsonk) remove this once we stop sending MediaType into tsc.
pub fn serialize_media_type<S>(mt: &MediaType, s: S) -> Result<S::Ok, S::Error>
where
  S: Serializer,
{
  s.serialize_str(&mt.to_string())
}
