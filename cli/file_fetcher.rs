// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.


use crate::http_cache::HttpCache;

use crate::media_type::MediaType;
use crate::text_encoding;

use deno_runtime::permissions::Permissions;

use deno_core::error::custom_error;
use deno_core::error::generic_error;
use deno_core::error::uri_error;
use deno_core::error::AnyError;


use deno_core::ModuleSpecifier;

use std::collections::HashMap;
use std::fs;

use std::io::Read;
use std::path::PathBuf;

use std::sync::Arc;
use std::sync::Mutex;

pub const SUPPORTED_SCHEMES: [&str; 2] = ["data", "file"];

/// A structure representing a source file.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct File {
  /// The path to the local version of the source file.  For local files this
  /// will be the direct path to that file.  For remote files, it will be the
  /// path to the file in the HTTP cache.
  pub local: PathBuf,
  /// For remote files, if there was an `X-TypeScript-Type` header, the parsed
  /// out value of that header.
  pub maybe_types: Option<String>,
  /// The resolved media type for the file.
  pub media_type: MediaType,
  /// The source of the file as a string.
  pub source: String,
  /// The _final_ specifier for the file.  The requested specifier and the final
  /// specifier maybe different for remote files that have been redirected.
  pub specifier: ModuleSpecifier,
}

/// Simple struct implementing in-process caching to prevent multiple
/// fs reads/net fetches for same file.
#[derive(Clone, Default)]
struct FileCache(Arc<Mutex<HashMap<ModuleSpecifier, File>>>);

impl FileCache {
  pub fn get(&self, specifier: &ModuleSpecifier) -> Option<File> {
    let cache = self.0.lock().unwrap();
    cache.get(specifier).cloned()
  }

  pub fn insert(&self, specifier: ModuleSpecifier, file: File) -> Option<File> {
    let mut cache = self.0.lock().unwrap();
    cache.insert(specifier, file)
  }
}

/// Indicates how cached source files should be handled.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum CacheSetting {
  /// Only the cached files should be used.  Any files not in the cache will
  /// error.  This is the equivalent of `--cached-only` in the CLI.
  Only,
  /// No cached source files should be used, and all files should be reloaded.
  /// This is the equivalent of `--reload` in the CLI.
  ReloadAll,
  /// Only some cached resources should be used.  This is the equivalent of
  /// `--reload=https://deno.land/std` or
  /// `--reload=https://deno.land/std,https://deno.land/x/example`.
  ReloadSome(Vec<String>),
  /// The cached source files should be used for local modules.  This is the
  /// default behavior of the CLI.
  Use,
}

impl CacheSetting {
  /// Returns if the cache should be used for a given specifier.
  pub fn should_use(&self, specifier: &ModuleSpecifier) -> bool {
    match self {
      CacheSetting::ReloadAll => false,
      CacheSetting::Use | CacheSetting::Only => true,
      CacheSetting::ReloadSome(list) => {
        let mut url = specifier.as_url().clone();
        url.set_fragment(None);
        if list.contains(&url.as_str().to_string()) {
          return false;
        }
        url.set_query(None);
        let mut path = PathBuf::from(url.as_str());
        loop {
          if list.contains(&path.to_str().unwrap().to_string()) {
            return false;
          }
          if !path.pop() {
            break;
          }
        }
        true
      }
    }
  }
}

/// Fetch a source file from the local file system.
fn fetch_local(specifier: &ModuleSpecifier) -> Result<File, AnyError> {
  let local = specifier.as_url().to_file_path().map_err(|_| {
    uri_error(format!("Invalid file path.\n  Specifier: {}", specifier))
  })?;
  let bytes = fs::read(local.clone())?;
  let charset = text_encoding::detect_charset(&bytes).to_string();
  let source = strip_shebang(get_source_from_bytes(bytes, Some(charset))?);
  let media_type = MediaType::from(specifier);

  Ok(File {
    local,
    maybe_types: None,
    media_type,
    source,
    specifier: specifier.clone(),
  })
}

/// Given a vector of bytes and optionally a charset, decode the bytes to a
/// string.
pub fn get_source_from_bytes(
  bytes: Vec<u8>,
  maybe_charset: Option<String>,
) -> Result<String, AnyError> {
  let source = if let Some(charset) = maybe_charset {
    text_encoding::convert_to_utf8(&bytes, &charset)?.to_string()
  } else {
    String::from_utf8(bytes)?
  };

  Ok(source)
}

fn get_source_from_data_url(
  specifier: &ModuleSpecifier,
) -> Result<(String, MediaType, String), AnyError> {
  let url = specifier.as_url();
  if url.scheme() != "data" {
    return Err(custom_error(
      "BadScheme",
      format!("Unexpected scheme of \"{}\"", url.scheme()),
    ));
  }
  let path = url.path();
  let mut parts = path.splitn(2, ',');
  let media_type_part =
    percent_encoding::percent_decode_str(parts.next().unwrap())
      .decode_utf8()?;
  let data_part = if let Some(data) = parts.next() {
    data
  } else {
    return Err(custom_error(
      "BadUrl",
      "The data URL is badly formed, missing a comma.",
    ));
  };
  let (media_type, maybe_charset) =
    map_content_type(specifier, Some(media_type_part.to_string()));
  let is_base64 = media_type_part.rsplit(';').any(|p| p == "base64");
  let bytes = if is_base64 {
    base64::decode(data_part)?
  } else {
    percent_encoding::percent_decode_str(data_part).collect()
  };
  let source = strip_shebang(get_source_from_bytes(bytes, maybe_charset)?);
  Ok((source, media_type, media_type_part.to_string()))
}

/// Return a validated scheme for a given module specifier.
fn get_validated_scheme(
  specifier: &ModuleSpecifier,
) -> Result<String, AnyError> {
  let scheme = specifier.as_url().scheme();
  if !SUPPORTED_SCHEMES.contains(&scheme) {
    Err(generic_error(format!(
      "Unsupported scheme \"{}\" for module \"{}\". Supported schemes: {:#?}",
      scheme, specifier, SUPPORTED_SCHEMES
    )))
  } else {
    Ok(scheme.to_string())
  }
}

/// Resolve a media type and optionally the charset from a module specifier and
/// the value of a content type header.
pub fn map_content_type(
  specifier: &ModuleSpecifier,
  maybe_content_type: Option<String>,
) -> (MediaType, Option<String>) {
  if let Some(content_type) = maybe_content_type {
    let mut content_types = content_type.split(';');
    let content_type = content_types.next().unwrap();
    let media_type = match content_type.trim().to_lowercase().as_ref() {
      "application/javascript"
      | "text/javascript"
      | "application/ecmascript"
      | "text/ecmascript"
      | "application/x-javascript"
      | "application/node" => {
        map_js_like_extension(specifier, MediaType::JavaScript)
      }
      "application/json" | "text/json" => MediaType::Json,
      "application/wasm" => MediaType::Wasm,
      // Handle plain and possibly webassembly
      "text/plain" | "application/octet-stream" => MediaType::from(specifier),
      _ => {
        debug!("unknown content type: {}", content_type);
        MediaType::Unknown
      }
    };
    let charset = content_types
      .map(str::trim)
      .find_map(|s| s.strip_prefix("charset="))
      .map(String::from);

    (media_type, charset)
  } else {
    (MediaType::from(specifier), None)
  }
}

/// Used to augment media types by using the path part of a module specifier to
/// resolve to a more accurate media type.
fn map_js_like_extension(
  specifier: &ModuleSpecifier,
  default: MediaType,
) -> MediaType {
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
  match path.extension() {
    None => default,
    Some(os_str) => match os_str.to_str() {
      None => default,
      Some(_) => default,
    },
  }
}

/// Remove shebangs from the start of source code strings
fn strip_shebang(mut value: String) -> String {
  if value.starts_with("#!") {
    if let Some(mid) = value.find('\n') {
      let (_, rest) = value.split_at(mid);
      value = rest.to_string()
    } else {
      value.clear()
    }
  }
  value
}

/// A structure for resolving, fetching and caching source files.
#[derive(Clone)]
pub struct FileFetcher {
  allow_remote: bool,
  cache: FileCache,
  cache_setting: CacheSetting,
  http_cache: HttpCache,
}

impl FileFetcher {
  pub fn new(
    http_cache: HttpCache,
    cache_setting: CacheSetting,
    allow_remote: bool,
  ) -> Result<Self, AnyError> {
    Ok(Self {
      allow_remote,
      cache: FileCache::default(),
      cache_setting,
      http_cache
    })
  }

  /// Creates a `File` structure for a remote file.
  fn build_remote_file(
    &self,
    specifier: &ModuleSpecifier,
    bytes: Vec<u8>,
    headers: &HashMap<String, String>,
  ) -> Result<File, AnyError> {
    let local = self.http_cache.get_cache_filename(specifier.as_url());
    let maybe_content_type = headers.get("content-type").cloned();
    let (media_type, maybe_charset) =
      map_content_type(specifier, maybe_content_type);
    let source = strip_shebang(get_source_from_bytes(bytes, maybe_charset)?);
    let maybe_types = headers.get("x-typescript-types").cloned();

    Ok(File {
      local,
      maybe_types,
      media_type,
      source,
      specifier: specifier.clone(),
    })
  }

  /// Fetch cached remote file.
  ///
  /// This is a recursive operation if source file has redirections.
  fn fetch_cached(
    &self,
    specifier: &ModuleSpecifier,
    redirect_limit: i64,
  ) -> Result<Option<File>, AnyError> {
    debug!("FileFetcher::fetch_cached - specifier: {}", specifier);
    if redirect_limit < 0 {
      return Err(custom_error("Http", "Too many redirects."));
    }

    let (mut source_file, headers) =
      match self.http_cache.get(specifier.as_url()) {
        Err(err) => {
          if let Some(err) = err.downcast_ref::<std::io::Error>() {
            if err.kind() == std::io::ErrorKind::NotFound {
              return Ok(None);
            }
          }
          return Err(err);
        }
        Ok(cache) => cache,
      };
    if let Some(redirect_to) = headers.get("location") {
      let redirect =
        ModuleSpecifier::resolve_import(redirect_to, specifier.as_str())?;
      return self.fetch_cached(&redirect, redirect_limit - 1);
    }
    let mut bytes = Vec::new();
    source_file.read_to_end(&mut bytes)?;
    let file = self.build_remote_file(specifier, bytes, &headers)?;

    Ok(Some(file))
  }

  /// Convert a data URL into a file, resulting in an error if the URL is
  /// invalid.
  fn fetch_data_url(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Result<File, AnyError> {
    debug!("FileFetcher::fetch_data_url() - specifier: {}", specifier);
    match self.fetch_cached(specifier, 0) {
      Ok(Some(file)) => return Ok(file),
      Ok(None) => {}
      Err(err) => return Err(err),
    }

    if self.cache_setting == CacheSetting::Only {
      return Err(custom_error(
        "NotFound",
        format!(
          "Specifier not found in cache: \"{}\", --cached-only is specified.",
          specifier
        ),
      ));
    }

    let (source, media_type, content_type) =
      get_source_from_data_url(specifier)?;
    let local = self.http_cache.get_cache_filename(specifier.as_url());
    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), content_type);
    self
      .http_cache
      .set(specifier.as_url(), headers, source.as_bytes())?;

    Ok(File {
      local,
      maybe_types: None,
      media_type,
      source,
      specifier: specifier.clone(),
    })
  }

  /// Fetch a source file and asynchronously return it.
  pub async fn fetch(
    &self,
    specifier: &ModuleSpecifier,
    permissions: &Permissions,
  ) -> Result<File, AnyError> {
    debug!("FileFetcher::fetch() - specifier: {}", specifier);
    let scheme = get_validated_scheme(specifier)?;
    permissions.check_specifier(specifier)?;
    if let Some(file) = self.cache.get(specifier) {
      Ok(file)
    } else if scheme == "file" {
      // we do not in memory cache files, as this would prevent files on the
      // disk changing effecting things like workers and dynamic imports.
      fetch_local(specifier)
    } else if scheme == "data" {
      let result = self.fetch_data_url(specifier);
      if let Ok(file) = &result {
        self.cache.insert(specifier.clone(), file.clone());
      }
      result
    } else {
      Err(custom_error(
        "NoRemote",
        format!("A remote specifier was requested: \"{}\", but --no-remote is specified.", specifier),
      ))
    }
  }

  /// Get the location of the current HTTP cache associated with the fetcher.
  pub fn get_http_cache_location(&self) -> PathBuf {
    self.http_cache.location.clone()
  }

  /// A synchronous way to retrieve a source file, where if the file has already
  /// been cached in memory it will be returned, otherwise for local files will
  /// be read from disk.
  pub fn get_source(&self, specifier: &ModuleSpecifier) -> Option<File> {
    let maybe_file = self.cache.get(specifier);
    if maybe_file.is_none() {
      let is_local = specifier.as_url().scheme() == "file";
      if is_local {
        if let Ok(file) = fetch_local(specifier) {
          return Some(file);
        }
      }
      None
    } else {
      maybe_file
    }
  }

  /// Insert a temporary module into the in memory cache for the file fetcher.
  pub fn insert_cached(&self, file: File) -> Option<File> {
    self.cache.insert(file.specifier.clone(), file)
  }
}
