// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use crate::ast;
use crate::ast::parse;
use crate::ast::transpile_module;

use crate::ast::Location;
use crate::ast::ParsedModule;
use crate::colors;
use crate::diagnostics::Diagnostics;
use crate::import_map::ImportMap;
use crate::info::ModuleGraphInfo;
use crate::info::ModuleInfo;
use crate::info::ModuleInfoMap;
use crate::info::ModuleInfoMapItem;
use crate::lockfile::Lockfile;
use crate::media_type::MediaType;
use crate::specifier_handler::CachedModule;
use crate::specifier_handler::Dependency;
use crate::specifier_handler::DependencyMap;
use crate::specifier_handler::Emit;
use crate::specifier_handler::FetchFuture;
use crate::specifier_handler::SpecifierHandler;

use deno_core::error::AnyError;

use deno_core::error::anyhow;
use deno_core::error::custom_error;
use deno_core::error::Context;
use deno_core::futures::stream::FuturesUnordered;
use deno_core::futures::stream::StreamExt;
use deno_core::serde::Deserialize;
use deno_core::serde::Deserializer;
use deno_core::serde::Serialize;
use deno_core::serde::Serializer;

use deno_core::serde_json::Value;
use deno_core::ModuleResolutionError;
use deno_core::ModuleSource;
use deno_core::ModuleSpecifier;
use regex::Regex;
use std::collections::HashSet;
use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;
use std::result;
use std::sync::Arc;
use std::sync::Mutex;


lazy_static! {
  /// Matched the `@deno-types` pragma.
  static ref DENO_TYPES_RE: Regex =
    Regex::new(r#"(?i)^\s*@deno-types\s*=\s*(?:["']([^"']+)["']|(\S+))"#)
      .unwrap();
  /// Matches a `/// <reference ... />` comment reference.
  static ref TRIPLE_SLASH_REFERENCE_RE: Regex =
    Regex::new(r"(?i)^/\s*<reference\s.*?/>").unwrap();
  /// Matches a path reference, which adds a dependency to a module
  static ref PATH_REFERENCE_RE: Regex =
    Regex::new(r#"(?i)\spath\s*=\s*["']([^"']*)["']"#).unwrap();
  /// Matches a types reference, which for JavaScript files indicates the
  /// location of types to use when type checking a program that includes it as
  /// a dependency.
  static ref TYPES_REFERENCE_RE: Regex =
    Regex::new(r#"(?i)\stypes\s*=\s*["']([^"']*)["']"#).unwrap();
}

/// A group of errors that represent errors that can occur when interacting with
/// a module graph.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum GraphError {
  /// A module using the HTTPS protocol is trying to import a module with an
  /// HTTP schema.
  InvalidDowngrade(ModuleSpecifier, Location),
  /// A remote module is trying to import a local module.
  InvalidLocalImport(ModuleSpecifier, Location),
  /// The source code is invalid, as it does not match the expected hash in the
  /// lockfile.
  InvalidSource(ModuleSpecifier, PathBuf),
  /// An unexpected dependency was requested for a module.
  MissingDependency(ModuleSpecifier, String),
  /// An unexpected specifier was requested.
  MissingSpecifier(ModuleSpecifier),
  /// The current feature is not supported.
  NotSupported(String),
  /// A unsupported media type was attempted to be imported as a module.
  UnsupportedImportType(ModuleSpecifier, MediaType),
}

impl fmt::Display for GraphError {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match self {
      GraphError::InvalidDowngrade(ref specifier, ref location) => write!(f, "Modules imported via https are not allowed to import http modules.\n  Importing: {}\n    at {}", specifier, location),
      GraphError::InvalidLocalImport(ref specifier, ref location) => write!(f, "Remote modules are not allowed to import local modules.  Consider using a dynamic import instead.\n  Importing: {}\n    at {}", specifier, location),
      GraphError::InvalidSource(ref specifier, ref lockfile) => write!(f, "The source code is invalid, as it does not match the expected hash in the lock file.\n  Specifier: {}\n  Lock file: {}", specifier, lockfile.to_str().unwrap()),
      GraphError::MissingDependency(ref referrer, specifier) => write!(
        f,
        "The graph is missing a dependency.\n  Specifier: {} from {}",
        specifier, referrer
      ),
      GraphError::MissingSpecifier(ref specifier) => write!(
        f,
        "The graph is missing a specifier.\n  Specifier: {}",
        specifier
      ),
      GraphError::NotSupported(ref msg) => write!(f, "{}", msg),
      GraphError::UnsupportedImportType(ref specifier, ref media_type) => write!(f, "An unsupported media type was attempted to be imported as a module.\n  Specifier: {}\n  MediaType: {}", specifier, media_type),
    }
  }
}

impl Error for GraphError {}

/// A structure for handling bundle loading, which is implemented here, to
/// avoid a circular dependency with `ast`.
struct BundleLoader<'a> {
  cm: Rc<swc_common::SourceMap>,
  emit_options: &'a ast::EmitOptions,
  globals: &'a swc_common::Globals,
  graph: &'a Graph,
}

impl swc_bundler::Load for BundleLoader<'_> {
  fn load(
    &self,
    file: &swc_common::FileName,
  ) -> Result<swc_bundler::ModuleData, AnyError> {
    match file {
      swc_common::FileName::Custom(filename) => {
        let specifier = ModuleSpecifier::resolve_url_or_path(filename)
          .context("Failed to convert swc FileName to ModuleSpecifier.")?;
        if let Some(src) = self.graph.get_source(&specifier) {
          let media_type = self
            .graph
            .get_media_type(&specifier)
            .context("Looking up media type during bundling.")?;
          let (source_file, module) = transpile_module(
            filename,
            &src,
            &media_type,
            self.emit_options,
            self.globals,
            self.cm.clone(),
          )?;
          Ok(swc_bundler::ModuleData {
            fm: source_file,
            module,
            helpers: Default::default(),
          })
        } else {
          Err(
            GraphError::MissingDependency(specifier, "<bundle>".to_string())
              .into(),
          )
        }
      }
      _ => unreachable!("Received request for unsupported filename {:?}", file),
    }
  }
}

/// Determine if a comment contains a `@deno-types` pragma and optionally return
/// its value.
pub fn parse_deno_types(comment: &str) -> Option<String> {
  if let Some(captures) = DENO_TYPES_RE.captures(comment) {
    if let Some(m) = captures.get(1) {
      Some(m.as_str().to_string())
    } else if let Some(m) = captures.get(2) {
      Some(m.as_str().to_string())
    } else {
      panic!("unreachable");
    }
  } else {
    None
  }
}

/// A logical representation of a module within a graph.
#[derive(Debug, Clone)]
pub struct Module {
  pub dependencies: DependencyMap,
  is_dirty: bool,
  is_parsed: bool,
  maybe_emit: Option<Emit>,
  maybe_emit_path: Option<(PathBuf, Option<PathBuf>)>,
  maybe_import_map: Option<Arc<Mutex<ImportMap>>>,
  maybe_types: Option<(String, ModuleSpecifier)>,
  maybe_version: Option<String>,
  media_type: MediaType,
  specifier: ModuleSpecifier,
  source: String,
  source_path: PathBuf,
}

impl Default for Module {
  fn default() -> Self {
    Module {
      dependencies: HashMap::new(),
      is_dirty: false,
      is_parsed: false,
      maybe_emit: None,
      maybe_emit_path: None,
      maybe_import_map: None,
      maybe_types: None,
      maybe_version: None,
      media_type: MediaType::Unknown,
      specifier: ModuleSpecifier::resolve_url("file:///example.js").unwrap(),
      source: "".to_string(),
      source_path: PathBuf::new(),
    }
  }
}

impl Module {
  pub fn new(
    cached_module: CachedModule,
    is_root: bool,
    maybe_import_map: Option<Arc<Mutex<ImportMap>>>,
  ) -> Self {
    // If this is a local root file, and its media type is unknown, set the
    // media type to JavaScript.  This allows easier ability to create "shell"
    // scripts with Deno.
    let media_type = if is_root
      && !cached_module.is_remote
      && cached_module.media_type == MediaType::Unknown
    {
      MediaType::JavaScript
    } else {
      cached_module.media_type
    };
    let mut module = Module {
      specifier: cached_module.specifier,
      maybe_import_map,
      media_type,
      source: cached_module.source,
      source_path: cached_module.source_path,
      maybe_emit: cached_module.maybe_emit,
      maybe_emit_path: cached_module.maybe_emit_path,
      maybe_version: cached_module.maybe_version,
      is_dirty: false,
      ..Self::default()
    };
    if module.maybe_import_map.is_none() {
      if let Some(dependencies) = cached_module.maybe_dependencies {
        module.dependencies = dependencies;
        module.is_parsed = true;
      }
    }
    module.maybe_types = if let Some(ref specifier) = cached_module.maybe_types
    {
      Some((
        specifier.clone(),
        module
          .resolve_import(&specifier, None)
          .expect("could not resolve module"),
      ))
    } else {
      None
    };
    module
  }

  /// Parse a module, populating the structure with data retrieved from the
  /// source of the module.
  pub fn parse(&mut self) -> Result<ParsedModule, AnyError> {
    let parsed_module =
      parse(self.specifier.as_str(), &self.source, &self.media_type)?;

    // parse out any triple slash references
    // ignore: /// <reference path="..." />
    // https://www.typescriptlang.org/docs/handbook/triple-slash-directives.html

    // Parse out all the syntactical dependencies for a module
    let dependencies = parsed_module.analyze_dependencies();
    for desc in dependencies.iter().filter(|desc| {
      desc.kind != swc_ecmascript::dep_graph::DependencyKind::Require
    }) {
      let location = Location {
        filename: self.specifier.to_string(),
        col: desc.col,
        line: desc.line,
      };

      // In situations where there is a potential issue with resolving the
      // import specifier, that ends up being a module resolution error for a
      // code dependency, we should not throw in the `ModuleGraph` but instead
      // wait until runtime and throw there, as with dynamic imports they need
      // to be catchable, which means they need to be resolved at runtime.
      let maybe_specifier =
        match self.resolve_import(&desc.specifier, Some(location.clone())) {
          Ok(specifier) => Some(specifier),
          Err(any_error) => {
            match any_error.downcast_ref::<ModuleResolutionError>() {
              Some(ModuleResolutionError::ImportPrefixMissing(_, _)) => None,
              _ => {
                return Err(any_error);
              }
            }
          }
        };

      // Parse out any `@deno-types` pragmas and modify dependency
      let maybe_type = if !desc.leading_comments.is_empty() {
        let comment = desc.leading_comments.last().unwrap();
        if let Some(deno_types) = parse_deno_types(&comment.text).as_ref() {
          Some(self.resolve_import(deno_types, Some(location.clone()))?)
        } else {
          None
        }
      } else {
        None
      };

      let dep = self
        .dependencies
        .entry(desc.specifier.to_string())
        .or_insert_with(|| Dependency::new(location));
      dep.is_dynamic = desc.is_dynamic;
      if let Some(specifier) = maybe_specifier {
        if desc.kind == swc_ecmascript::dep_graph::DependencyKind::ExportType
          || desc.kind == swc_ecmascript::dep_graph::DependencyKind::ImportType
        {
          dep.maybe_type = Some(specifier);
        } else {
          dep.maybe_code = Some(specifier);
        }
      }
      // If the dependency wasn't a type only dependency already, and there is
      // a `@deno-types` comment, then we will set the `maybe_type` dependency.
      if maybe_type.is_some() && dep.maybe_type.is_none() {
        dep.maybe_type = maybe_type;
      }
    }
    Ok(parsed_module)
  }

  fn resolve_import(
    &self,
    specifier: &str,
    maybe_location: Option<Location>,
  ) -> Result<ModuleSpecifier, AnyError> {
    let maybe_resolve = if let Some(import_map) = self.maybe_import_map.clone()
    {
      import_map
        .lock()
        .unwrap()
        .resolve(specifier, self.specifier.as_str())?
    } else {
      None
    };
    let mut remapped_import = false;
    let specifier = if let Some(module_specifier) = maybe_resolve {
      remapped_import = true;
      module_specifier
    } else {
      ModuleSpecifier::resolve_import(specifier, self.specifier.as_str())?
    };

    let referrer_scheme = self.specifier.as_url().scheme();
    let specifier_scheme = specifier.as_url().scheme();
    let location = maybe_location.unwrap_or(Location {
      filename: self.specifier.to_string(),
      line: 0,
      col: 0,
    });

    // Disallow downgrades from HTTPS to HTTP
    if referrer_scheme == "https" && specifier_scheme == "http" {
      return Err(
        GraphError::InvalidDowngrade(specifier.clone(), location).into(),
      );
    }

    // Disallow a remote URL from trying to import a local URL, unless it is a
    // remapped import via the import map
    if (referrer_scheme == "https" || referrer_scheme == "http")
      && !(specifier_scheme == "https" || specifier_scheme == "http")
      && !remapped_import
    {
      return Err(
        GraphError::InvalidLocalImport(specifier.clone(), location).into(),
      );
    }

    Ok(specifier)
  }

  pub fn size(&self) -> usize {
    self.source.as_bytes().len()
  }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Stats(pub Vec<(String, u32)>);

impl<'de> Deserialize<'de> for Stats {
  fn deserialize<D>(deserializer: D) -> result::Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let items: Vec<(String, u32)> = Deserialize::deserialize(deserializer)?;
    Ok(Stats(items))
  }
}

impl Serialize for Stats {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    Serialize::serialize(&self.0, serializer)
  }
}

impl fmt::Display for Stats {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    writeln!(f, "Compilation statistics:")?;
    for (key, value) in self.0.clone() {
      writeln!(f, "  {}: {}", key, value)?;
    }

    Ok(())
  }
}

/// A structure that provides information about a module graph result.
#[derive(Debug, Default)]
pub struct ResultInfo {
  /// A structure which provides diagnostic information (usually from `tsc`)
  /// about the code in the module graph.
  pub diagnostics: Diagnostics,
  /// A map of specifiers to the result of their resolution in the module graph.
  pub loadable_modules:
    HashMap<ModuleSpecifier, Result<ModuleSource, AnyError>>,
  /// Optionally ignored compiler options that represent any options that were
  /// ignored if there was a user provided configuration.
  // pub maybe_ignored_options: Option<IgnoredCompilerOptions>,
  /// A structure providing key metrics around the operation performed, in
  /// milliseconds.
  pub stats: Stats,
}

/// Represents the "default" type library that should be used when type
/// checking the code in the module graph.  Note that a user provided config
/// of `"lib"` would override this value.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TypeLib {
  DenoWindow,
  DenoWorker,
  UnstableDenoWindow,
  UnstableDenoWorker,
}

impl Default for TypeLib {
  fn default() -> Self {
    TypeLib::DenoWindow
  }
}

impl Serialize for TypeLib {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    let value = match self {
      TypeLib::DenoWindow => vec!["deno.window".to_string()],
      TypeLib::DenoWorker => vec!["deno.worker".to_string()],
      TypeLib::UnstableDenoWindow => {
        vec!["deno.window".to_string(), "deno.unstable".to_string()]
      }
      TypeLib::UnstableDenoWorker => {
        vec!["deno.worker".to_string(), "deno.unstable".to_string()]
      }
    };
    Serialize::serialize(&value, serializer)
  }
}

#[derive(Debug, Default)]
pub struct BundleOptions {
  /// If `true` then debug logging will be output from the isolate.
  pub debug: bool,
  /// An optional string that points to a user supplied TypeScript configuration
  /// file that augments the the default configuration passed to the TypeScript
  /// compiler.
  pub maybe_config_path: Option<String>,
}

#[derive(Debug, Default)]
pub struct CheckOptions {
  /// If `true` then debug logging will be output from the isolate.
  pub debug: bool,
  /// Utilise the emit from `tsc` to update the emitted code for modules.
  pub emit: bool,
  /// The base type libraries that should be used when type checking.
  pub lib: TypeLib,
  /// An optional string that points to a user supplied TypeScript configuration
  /// file that augments the the default configuration passed to the TypeScript
  /// compiler.
  pub maybe_config_path: Option<String>,
  /// Ignore any previously emits and ensure that all files are emitted from
  /// source.
  pub reload: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub enum BundleType {
  /// Return the emitted contents of the program as a single "flattened" ES
  /// module.
  #[allow(warnings)]
  Esm,
  // TODO(@kitsonk) once available in swc
  // Iife,
  /// Do not bundle the emit, instead returning each of the modules that are
  /// part of the program as individual files.
  None,
}

impl Default for BundleType {
  fn default() -> Self {
    BundleType::None
  }
}

#[derive(Debug, Default)]
pub struct EmitOptions {
  /// If true, then code will be type checked, otherwise type checking will be
  /// skipped.  If false, then swc will be used for the emit, otherwise tsc will
  /// be used.
  pub check: bool,
  /// Indicate the form the result of the emit should take.
  pub bundle_type: BundleType,
  /// If `true` then debug logging will be output from the isolate.
  pub debug: bool,
  /// An optional map that contains user supplied TypeScript compiler
  /// configuration options that are passed to the TypeScript compiler.
  pub maybe_user_config: Option<HashMap<String, Value>>,
}

/// A structure which provides options when transpiling modules.
#[derive(Debug, Default)]
pub struct TranspileOptions {
  /// If `true` then debug logging will be output from the isolate.
  pub debug: bool,
  /// An optional string that points to a user supplied TypeScript configuration
  /// file that augments the the default configuration passed to the TypeScript
  /// compiler.
  pub maybe_config_path: Option<String>,
  /// Ignore any previously emits and ensure that all files are emitted from
  /// source.
  pub reload: bool,
}

#[derive(Debug, Clone)]
enum ModuleSlot {
  /// The module fetch resulted in a non-recoverable error.
  Err(Arc<AnyError>),
  /// The the fetch resulted in a module.
  Module(Box<Module>),
  /// Used to denote a module that isn't part of the graph.
  None,
  /// The fetch of the module is pending.
  Pending,
}

/// A dependency graph of modules, were the modules that have been inserted via
/// the builder will be loaded into the graph.  Also provides an interface to
/// be able to manipulate and handle the graph.
#[derive(Debug, Clone)]
pub struct Graph {
  /// A reference to the specifier handler that will retrieve and cache modules
  /// for the graph.
  handler: Arc<Mutex<dyn SpecifierHandler>>,
  /// Optional TypeScript build info that will be passed to `tsc` if `tsc` is
  /// invoked.
  maybe_tsbuildinfo: Option<String>,
  /// The modules that are part of the graph.
  modules: HashMap<ModuleSpecifier, ModuleSlot>,
  /// A map of redirects, where a module specifier is redirected to another
  /// module specifier by the handler.  All modules references should be
  /// resolved internally via this, before attempting to access the module via
  /// the handler, to make sure the correct modules is being dealt with.
  redirects: HashMap<ModuleSpecifier, ModuleSpecifier>,
  /// The module specifiers that have been uniquely added to the graph, which
  /// does not include any transient dependencies.
  roots: Vec<ModuleSpecifier>,
  /// If all of the root modules are dynamically imported, then this is true.
  /// This is used to ensure correct `--reload` behavior, where subsequent
  /// calls to a module graph where the emit is already valid do not cause the
  /// graph to re-emit.
  roots_dynamic: bool,
  // A reference to lock file that will be used to check module integrity.
  maybe_lockfile: Option<Arc<Mutex<Lockfile>>>,
}

/// Convert a specifier and a module slot in a result to the module source which
/// is needed by Deno core for loading the module.
fn to_module_result(
  (specifier, module_slot): (&ModuleSpecifier, &ModuleSlot),
) -> (ModuleSpecifier, Result<ModuleSource, AnyError>) {
  match module_slot {
    ModuleSlot::Err(err) => (specifier.clone(), Err(anyhow!(err.to_string()))),
    ModuleSlot::Module(module) => (
      specifier.clone(),
      if let Some(emit) = &module.maybe_emit {
        match emit {
          Emit::Cli((code, _)) => Ok(ModuleSource {
            code: code.clone(),
            module_url_found: module.specifier.to_string(),
            module_url_specified: specifier.to_string(),
          }),
        }
      } else {
        match module.media_type {
          MediaType::JavaScript | MediaType::Unknown => Ok(ModuleSource {
            code: module.source.clone(),
            module_url_found: module.specifier.to_string(),
            module_url_specified: specifier.to_string(),
          }),
          _ => Err(custom_error(
            "NotFound",
            format!("Compiled module not found \"{}\"", specifier),
          )),
        }
      },
    ),
    _ => (
      specifier.clone(),
      Err(anyhow!("Module \"{}\" unavailable.", specifier)),
    ),
  }
}

impl Graph {
  /// Create a new instance of a graph, ready to have modules loaded it.
  ///
  /// The argument `handler` is an instance of a structure that implements the
  /// `SpecifierHandler` trait.
  ///
  pub fn new(
    handler: Arc<Mutex<dyn SpecifierHandler>>,
    maybe_lockfile: Option<Arc<Mutex<Lockfile>>>,
  ) -> Self {
    Graph {
      handler,
      maybe_tsbuildinfo: None,
      modules: HashMap::new(),
      redirects: HashMap::new(),
      roots: Vec::new(),
      roots_dynamic: true,
      maybe_lockfile,
    }
  }

  /// Type check the module graph, corresponding to the options provided.
  pub fn check(self, _options: CheckOptions) -> Result<ResultInfo, AnyError> {
    // let mut config = TsConfig::new(json!({
    //   "allowJs": true,
    //   // TODO(@kitsonk) is this really needed?
    //   "esModuleInterop": true,
    //   // Enabled by default to align to transpile/swc defaults
    //   "experimentalDecorators": true,
    //   "incremental": true,
    //   "isolatedModules": true,
    //   "lib": options.lib,
    //   "module": "esnext",
    //   "strict": true,
    //   "target": "esnext",
    //   "tsBuildInfoFile": "deno:///.tsbuildinfo",
    // }));
    // if options.emit {
    //   config.merge(&json!({
    //     // TODO(@kitsonk) consider enabling this by default
    //     //   see: https://github.com/denoland/deno/issues/7732
    //     "emitDecoratorMetadata": false,
    //     "jsx": "react",
    //     "inlineSourceMap": true,
    //     "outDir": "deno://",
    //     "removeComments": true,
    //   }));
    // } else {
    //   config.merge(&json!({
    //     "noEmit": true,
    //   }));
    // }
    // TODO: to be deleted
    // let maybe_ignored_options =
    //   config.merge_tsconfig(options.maybe_config_path)?;

    return Ok(ResultInfo {
      // maybe_ignored_options,
      loadable_modules: self.get_loadable_modules(),
      ..Default::default()
    });
  }

  fn get_info(
    &self,
    specifier: &ModuleSpecifier,
    seen: &mut HashSet<ModuleSpecifier>,
    totals: &mut HashMap<ModuleSpecifier, usize>,
  ) -> ModuleInfo {
    let not_seen = seen.insert(specifier.clone());
    let module = match self.get_module(specifier) {
      ModuleSlot::Module(module) => module,
      ModuleSlot::Err(err) => {
        error!("{}: {}", colors::red_bold("error"), err.to_string());
        std::process::exit(1);
      }
      _ => unreachable!(),
    };
    let mut deps = Vec::new();
    let mut total_size = None;

    if not_seen {
      let mut seen_deps = HashSet::new();
      // TODO(@kitsonk) https://github.com/denoland/deno/issues/7927
      for (_, dep) in module.dependencies.iter() {
        // Check the runtime code dependency
        if let Some(code_dep) = &dep.maybe_code {
          if seen_deps.insert(code_dep.clone()) {
            deps.push(self.get_info(code_dep, seen, totals));
          }
        }
      }
      deps.sort();
      total_size = if let Some(total) = totals.get(specifier) {
        Some(total.to_owned())
      } else {
        let mut total = deps
          .iter()
          .map(|d| {
            if let Some(total_size) = d.total_size {
              total_size
            } else {
              0
            }
          })
          .sum();
        total += module.size();
        totals.insert(specifier.clone(), total);
        Some(total)
      };
    }

    ModuleInfo {
      deps,
      name: specifier.clone(),
      size: module.size(),
      total_size,
    }
  }

  fn get_info_map(&self) -> ModuleInfoMap {
    let map = self
      .modules
      .iter()
      .filter_map(|(specifier, module_slot)| {
        if let ModuleSlot::Module(module) = module_slot {
          let mut deps = BTreeSet::new();
          for (_, dep) in module.dependencies.iter() {
            if let Some(code_dep) = &dep.maybe_code {
              deps.insert(code_dep.clone());
            }
            if let Some(type_dep) = &dep.maybe_type {
              deps.insert(type_dep.clone());
            }
          }
          if let Some((_, types_dep)) = &module.maybe_types {
            deps.insert(types_dep.clone());
          }
          let item = ModuleInfoMapItem {
            deps: deps.into_iter().collect(),
            size: module.size(),
          };
          Some((specifier.clone(), item))
        } else {
          None
        }
      })
      .collect();

    ModuleInfoMap::new(map)
  }

  /// Retrieve a map that contains a representation of each module in the graph
  /// which can be used to provide code to a module loader without holding all
  /// the state to be able to operate on the graph.
  pub fn get_loadable_modules(
    &self,
  ) -> HashMap<ModuleSpecifier, Result<ModuleSource, AnyError>> {
    let mut loadable_modules: HashMap<
      ModuleSpecifier,
      Result<ModuleSource, AnyError>,
    > = self.modules.iter().map(to_module_result).collect();
    for (specifier, _) in self.redirects.iter() {
      if let Some(module_slot) =
        self.modules.get(self.resolve_specifier(specifier))
      {
        let (_, result) = to_module_result((specifier, module_slot));
        loadable_modules.insert(specifier.clone(), result);
      }
    }
    loadable_modules
  }

  pub fn get_media_type(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Option<MediaType> {
    if let ModuleSlot::Module(module) = self.get_module(specifier) {
      Some(module.media_type)
    } else {
      None
    }
  }

  fn get_module(&self, specifier: &ModuleSpecifier) -> &ModuleSlot {
    let s = self.resolve_specifier(specifier);
    if let Some(module_slot) = self.modules.get(s) {
      module_slot
    } else {
      &ModuleSlot::None
    }
  }

  /// Consume graph and return list of all module specifiers contained in the
  /// graph.
  // pub fn get_modules(&self) -> Vec<ModuleSpecifier> {
  //   self.modules.keys().map(|s| s.to_owned()).collect()
  // }

  /// Get the source for a given module specifier.  If the module is not part
  /// of the graph, the result will be `None`.
  pub fn get_source(&self, specifier: &ModuleSpecifier) -> Option<String> {
    if let ModuleSlot::Module(module) = self.get_module(specifier) {
      Some(module.source.clone())
    } else {
      None
    }
  }

  /// Return a structure which provides information about the module graph and
  /// the relationship of the modules in the graph.  This structure is used to
  /// provide information for the `info` subcommand.
  pub fn info(&self) -> Result<ModuleGraphInfo, AnyError> {
    if self.roots.is_empty() || self.roots.len() > 1 {
      return Err(GraphError::NotSupported(format!("Info is only supported when there is a single root module in the graph.  Found: {}", self.roots.len())).into());
    }

    let module = self.roots[0].clone();
    let m = if let ModuleSlot::Module(module) = self.get_module(&module) {
      module
    } else {
      return Err(GraphError::MissingSpecifier(module.clone()).into());
    };

    let mut seen = HashSet::new();
    let mut totals = HashMap::new();
    let info = self.get_info(&module, &mut seen, &mut totals);

    let files = self.get_info_map();
    let total_size = totals.get(&module).unwrap_or(&m.size()).to_owned();
    let (compiled, map) =
      if let Some((emit_path, maybe_map_path)) = &m.maybe_emit_path {
        (Some(emit_path.clone()), maybe_map_path.clone())
      } else {
        (None, None)
      };

    let dep_count = self
      .modules
      .iter()
      .filter_map(|(_, m)| match m {
        ModuleSlot::Module(_) => Some(1),
        _ => None,
      })
      .count()
      - 1;

    Ok(ModuleGraphInfo {
      compiled,
      dep_count,
      file_type: m.media_type,
      files,
      info,
      local: m.source_path.clone(),
      map,
      module,
      total_size,
    })
  }

  /// Verify the subresource integrity of the graph based upon the optional
  /// lockfile, updating the lockfile with any missing resources.  This will
  /// error if any of the resources do not match their lock status.
  pub fn lock(&self) {
    if let Some(lf) = self.maybe_lockfile.as_ref() {
      let mut lockfile = lf.lock().unwrap();
      for (ms, module_slot) in self.modules.iter() {
        if let ModuleSlot::Module(module) = module_slot {
          let specifier = module.specifier.to_string();
          let valid = lockfile.check_or_insert(&specifier, &module.source);
          if !valid {
            eprintln!(
              "{}",
              GraphError::InvalidSource(ms.clone(), lockfile.filename.clone())
            );
            std::process::exit(10);
          }
        }
      }
    }
  }

  /// Given a string specifier and a referring module specifier, provide the
  /// resulting module specifier and media type for the module that is part of
  /// the graph.
  ///
  /// # Arguments
  ///
  /// * `specifier` - The string form of the module specifier that needs to be
  ///   resolved.
  /// * `referrer` - The referring `ModuleSpecifier`.
  /// * `prefer_types` - When resolving to a module specifier, determine if a
  ///   type dependency is preferred over a code dependency.  This is set to
  ///   `true` when resolving module names for `tsc` as it needs the type
  ///   dependency over the code, while other consumers do not handle type only
  ///   dependencies.
  pub fn resolve(
    &self,
    specifier: &str,
    referrer: &ModuleSpecifier,
    prefer_types: bool,
  ) -> Result<ModuleSpecifier, AnyError> {
    let module = if let ModuleSlot::Module(module) = self.get_module(referrer) {
      module
    } else {
      return Err(GraphError::MissingSpecifier(referrer.clone()).into());
    };
    if !module.dependencies.contains_key(specifier) {
      return Err(
        GraphError::MissingDependency(
          referrer.to_owned(),
          specifier.to_owned(),
        )
        .into(),
      );
    }
    let dependency = module.dependencies.get(specifier).unwrap();
    // If there is a @deno-types pragma that impacts the dependency, then the
    // maybe_type property will be set with that specifier, otherwise we use the
    // specifier that point to the runtime code.
    let resolved_specifier = if prefer_types && dependency.maybe_type.is_some()
    {
      dependency.maybe_type.clone().unwrap()
    } else if let Some(code_specifier) = dependency.maybe_code.clone() {
      code_specifier
    } else {
      return Err(
        GraphError::MissingDependency(
          referrer.to_owned(),
          specifier.to_owned(),
        )
        .into(),
      );
    };
    let dep_module = if let ModuleSlot::Module(dep_module) =
      self.get_module(&resolved_specifier)
    {
      dep_module
    } else {
      return Err(
        GraphError::MissingDependency(
          referrer.to_owned(),
          resolved_specifier.to_string(),
        )
        .into(),
      );
    };
    // In the case that there is a X-TypeScript-Types or a triple-slash types,
    // then the `maybe_types` specifier will be populated and we should use that
    // instead.
    let result = if prefer_types && dep_module.maybe_types.is_some() {
      let (_, types) = dep_module.maybe_types.clone().unwrap();
      // It is possible that `types` points to a redirected specifier, so we
      // need to ensure it resolves to the final specifier in the graph.
      self.resolve_specifier(&types).clone()
    } else {
      dep_module.specifier.clone()
    };

    Ok(result)
  }

  /// Takes a module specifier and returns the "final" specifier, accounting for
  /// any redirects that may have occurred.
  fn resolve_specifier<'a>(
    &'a self,
    specifier: &'a ModuleSpecifier,
  ) -> &'a ModuleSpecifier {
    let mut s = specifier;
    let mut seen = HashSet::new();
    seen.insert(s.clone());
    while let Some(redirect) = self.redirects.get(s) {
      if !seen.insert(redirect.clone()) {
        eprintln!("An infinite loop of module redirections detected.\n  Original specifier: {}", specifier);
        break;
      }
      s = redirect;
      if seen.len() > 5 {
        eprintln!("An excessive number of module redirections detected.\n  Original specifier: {}", specifier);
        break;
      }
    }
    s
  }
}

impl swc_bundler::Resolve for Graph {
  fn resolve(
    &self,
    referrer: &swc_common::FileName,
    specifier: &str,
  ) -> Result<swc_common::FileName, AnyError> {
    let referrer = if let swc_common::FileName::Custom(referrer) = referrer {
      ModuleSpecifier::resolve_url_or_path(referrer)
        .context("Cannot resolve swc FileName to a module specifier")?
    } else {
      unreachable!(
        "An unexpected referrer was passed when bundling: {:?}",
        referrer
      )
    };
    let specifier = self.resolve(specifier, &referrer, false)?;

    Ok(swc_common::FileName::Custom(specifier.to_string()))
  }
}

/// A structure for building a dependency graph of modules.
pub struct GraphBuilder {
  graph: Graph,
  maybe_import_map: Option<Arc<Mutex<ImportMap>>>,
  pending: FuturesUnordered<FetchFuture>,
}

impl GraphBuilder {
  pub fn new(
    handler: Arc<Mutex<dyn SpecifierHandler>>,
    maybe_import_map: Option<ImportMap>,
    maybe_lockfile: Option<Arc<Mutex<Lockfile>>>,
  ) -> Self {
    let internal_import_map = if let Some(import_map) = maybe_import_map {
      Some(Arc::new(Mutex::new(import_map)))
    } else {
      None
    };
    GraphBuilder {
      graph: Graph::new(handler, maybe_lockfile),
      maybe_import_map: internal_import_map,
      pending: FuturesUnordered::new(),
    }
  }

  /// Add a module into the graph based on a module specifier.  The module
  /// and any dependencies will be fetched from the handler.  The module will
  /// also be treated as a _root_ module in the graph.
  pub async fn add(
    &mut self,
    specifier: &ModuleSpecifier,
    is_dynamic: bool,
  ) -> Result<(), AnyError> {
    self.fetch(specifier, &None, is_dynamic);

    loop {
      match self.pending.next().await {
        Some(Err((specifier, err))) => {
          self
            .graph
            .modules
            .insert(specifier, ModuleSlot::Err(Arc::new(err)));
        }
        Some(Ok(cached_module)) => {
          let is_root = &cached_module.specifier == specifier;
          self.visit(cached_module, is_root)?;
        }
        _ => {}
      }
      if self.pending.is_empty() {
        break;
      }
    }

    if !self.graph.roots.contains(specifier) {
      self.graph.roots.push(specifier.clone());
      self.graph.roots_dynamic = self.graph.roots_dynamic && is_dynamic;
      if self.graph.maybe_tsbuildinfo.is_none() {
        let handler = self.graph.handler.lock().unwrap();
        self.graph.maybe_tsbuildinfo = handler.get_tsbuildinfo(specifier)?;
      }
    }

    Ok(())
  }

  /// Request a module to be fetched from the handler and queue up its future
  /// to be awaited to be resolved.
  fn fetch(
    &mut self,
    specifier: &ModuleSpecifier,
    maybe_referrer: &Option<Location>,
    is_dynamic: bool,
  ) {
    if !self.graph.modules.contains_key(&specifier) {
      self
        .graph
        .modules
        .insert(specifier.clone(), ModuleSlot::Pending);
      let mut handler = self.graph.handler.lock().unwrap();
      let future =
        handler.fetch(specifier.clone(), maybe_referrer.clone(), is_dynamic);
      self.pending.push(future);
    }
  }

  /// Visit a module that has been fetched, hydrating the module, analyzing its
  /// dependencies if required, fetching those dependencies, and inserting the
  /// module into the graph.
  fn visit(
    &mut self,
    cached_module: CachedModule,
    is_root: bool,
  ) -> Result<(), AnyError> {
    let specifier = cached_module.specifier.clone();
    let requested_specifier = cached_module.requested_specifier.clone();
    let mut module =
      Module::new(cached_module, is_root, self.maybe_import_map.clone());
    match module.media_type {
      MediaType::Json
      | MediaType::SourceMap
      | MediaType::Unknown => {
        return Err(
          GraphError::UnsupportedImportType(
            module.specifier,
            module.media_type,
          )
          .into(),
        );
      }
      _ => (),
    }
    if !module.is_parsed {
      let has_types = module.maybe_types.is_some();
      module.parse()?;
      if self.maybe_import_map.is_none() {
        let mut handler = self.graph.handler.lock().unwrap();
        handler.set_deps(&specifier, module.dependencies.clone())?;
        if !has_types {
          if let Some((types, _)) = module.maybe_types.clone() {
            handler.set_types(&specifier, types)?;
          }
        }
      }
    }
    for (_, dep) in module.dependencies.iter() {
      let maybe_referrer = Some(dep.location.clone());
      if let Some(specifier) = dep.maybe_code.as_ref() {
        self.fetch(specifier, &maybe_referrer, dep.is_dynamic);
      }
      if let Some(specifier) = dep.maybe_type.as_ref() {
        self.fetch(specifier, &maybe_referrer, dep.is_dynamic);
      }
    }
    if let Some((_, specifier)) = module.maybe_types.as_ref() {
      self.fetch(specifier, &None, false);
    }
    if specifier != requested_specifier {
      self
        .graph
        .redirects
        .insert(requested_specifier, specifier.clone());
    }
    self
      .graph
      .modules
      .insert(specifier, ModuleSlot::Module(Box::new(module)));

    Ok(())
  }

  /// Move out the graph from the builder to be utilized further.  An optional
  /// lockfile can be provided, where if the sources in the graph do not match
  /// the expected lockfile, an error will be logged and the process will exit.
  pub fn get_graph(self) -> Graph {
    self.graph.lock();
    self.graph
  }
}
