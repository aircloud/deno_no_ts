// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use crate::media_type::MediaType;


use deno_core::error::AnyError;

use deno_core::ModuleSpecifier;
use std::error::Error;
use std::fmt;

use std::rc::Rc;
use std::sync::Arc;
use std::sync::RwLock;
use swc_common::chain;
use swc_common::comments::Comment;

use swc_common::comments::SingleThreadedComments;
use swc_common::errors::Diagnostic;
use swc_common::errors::DiagnosticBuilder;
use swc_common::errors::Emitter;
use swc_common::errors::Handler;
use swc_common::errors::HandlerFlags;
use swc_common::FileName;
use swc_common::Globals;
use swc_common::Loc;
use swc_common::SourceFile;
use swc_common::SourceMap;
use swc_common::Span;
use swc_ecmascript::ast::Module;

use swc_ecmascript::dep_graph::analyze_dependencies;
use swc_ecmascript::dep_graph::DependencyDescriptor;
use swc_ecmascript::parser::lexer::Lexer;

use swc_ecmascript::parser::EsConfig;
use swc_ecmascript::parser::JscTarget;
use swc_ecmascript::parser::StringInput;
use swc_ecmascript::parser::Syntax;

use swc_ecmascript::transforms::fixer;
use swc_ecmascript::transforms::helpers;

use swc_ecmascript::transforms::pass::Optional;
use swc_ecmascript::transforms::proposals;
use swc_ecmascript::transforms::react;
use swc_ecmascript::transforms::typescript;
use swc_ecmascript::visit::FoldWith;

static TARGET: JscTarget = JscTarget::Es2020;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Location {
  pub filename: String,
  pub line: usize,
  pub col: usize,
}

impl Into<Location> for swc_common::Loc {
  fn into(self) -> Location {
    use swc_common::FileName::*;

    let filename = match &self.file.name {
      Real(path_buf) => path_buf.to_string_lossy().to_string(),
      Custom(str_) => str_.to_string(),
      _ => panic!("invalid filename"),
    };

    Location {
      filename,
      line: self.line,
      col: self.col_display,
    }
  }
}

impl Into<ModuleSpecifier> for Location {
  fn into(self) -> ModuleSpecifier {
    ModuleSpecifier::resolve_url_or_path(&self.filename).unwrap()
  }
}

impl std::fmt::Display for Location {
  fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    write!(f, "{}:{}:{}", self.filename, self.line, self.col)
  }
}

/// A buffer for collecting diagnostic messages from the AST parser.
#[derive(Debug)]
pub struct DiagnosticBuffer(Vec<String>);

impl Error for DiagnosticBuffer {}

impl fmt::Display for DiagnosticBuffer {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let s = self.0.join(",");
    f.pad(&s)
  }
}

impl DiagnosticBuffer {
  pub fn from_error_buffer<F>(error_buffer: ErrorBuffer, get_loc: F) -> Self
  where
    F: Fn(Span) -> Loc,
  {
    let s = error_buffer.0.read().unwrap().clone();
    let diagnostics = s
      .iter()
      .map(|d| {
        let mut msg = d.message();

        if let Some(span) = d.span.primary_span() {
          let loc = get_loc(span);
          let file_name = match &loc.file.name {
            FileName::Custom(n) => n,
            _ => unreachable!(),
          };
          msg = format!(
            "{} at {}:{}:{}",
            msg, file_name, loc.line, loc.col_display
          );
        }

        msg
      })
      .collect::<Vec<String>>();

    Self(diagnostics)
  }
}

/// A buffer for collecting errors from the AST parser.
#[derive(Debug, Clone)]
pub struct ErrorBuffer(Arc<RwLock<Vec<Diagnostic>>>);

impl ErrorBuffer {
  pub fn new() -> Self {
    Self(Arc::new(RwLock::new(Vec::new())))
  }
}

impl Emitter for ErrorBuffer {
  fn emit(&mut self, db: &DiagnosticBuilder) {
    self.0.write().unwrap().push((**db).clone());
  }
}

fn get_es_config(jsx: bool) -> EsConfig {
  EsConfig {
    class_private_methods: true,
    class_private_props: true,
    class_props: true,
    dynamic_import: true,
    export_default_from: true,
    export_namespace_from: true,
    import_meta: true,
    jsx,
    nullish_coalescing: true,
    num_sep: true,
    optional_chaining: true,
    top_level_await: true,
    ..EsConfig::default()
  }
}

pub fn get_syntax(media_type: &MediaType) -> Syntax {
  match media_type {
    MediaType::JavaScript => Syntax::Es(get_es_config(false)),
    _ => Syntax::Es(get_es_config(false)),
  }
}

/// Options which can be adjusted when transpiling a module.
#[derive(Debug, Clone)]
pub struct EmitOptions {
  /// Indicate if JavaScript is being checked/transformed as well, or if it is
  /// only TypeScript.
  pub check_js: bool,
  /// When emitting a legacy decorator, also emit experimental decorator meta
  /// data.  Defaults to `false`.
  pub emit_metadata: bool,
  /// Should the source map be inlined in the emitted code file, or provided
  /// as a separate file.  Defaults to `true`.
  pub inline_source_map: bool,
  /// When transforming JSX, what value should be used for the JSX factory.
  /// Defaults to `React.createElement`.
  pub jsx_factory: String,
  /// When transforming JSX, what value should be used for the JSX fragment
  /// factory.  Defaults to `React.Fragment`.
  pub jsx_fragment_factory: String,
  /// Should JSX be transformed or preserved.  Defaults to `true`.
  pub transform_jsx: bool,
}

impl Default for EmitOptions {
  fn default() -> Self {
    EmitOptions {
      check_js: false,
      emit_metadata: false,
      inline_source_map: true,
      jsx_factory: "React.createElement".into(),
      jsx_fragment_factory: "React.Fragment".into(),
      transform_jsx: true,
    }
  }
}


/// A logical structure to hold the value of a parsed module for further
/// processing.
#[derive(Clone)]
pub struct ParsedModule {
  comments: SingleThreadedComments,
  leading_comments: Vec<Comment>,
  module: Module,
  source_map: Rc<SourceMap>,
  source_file: Rc<SourceFile>,
}

impl fmt::Debug for ParsedModule {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    f.debug_struct("ParsedModule")
      .field("comments", &self.comments)
      .field("leading_comments", &self.leading_comments)
      .field("module", &self.module)
      .finish()
  }
}

impl ParsedModule {
  /// Return a vector of dependencies for the module.
  pub fn analyze_dependencies(&self) -> Vec<DependencyDescriptor> {
    analyze_dependencies(&self.module, &self.source_map, &self.comments)
  }
}

pub fn parse_with_source_map(
  specifier: &str,
  source: &str,
  media_type: &MediaType,
  source_map: Rc<SourceMap>,
) -> Result<ParsedModule, AnyError> {
  let source_file = source_map.new_source_file(
    FileName::Custom(specifier.to_string()),
    source.to_string(),
  );
  let error_buffer = ErrorBuffer::new();
  let syntax = get_syntax(media_type);
  let input = StringInput::from(&*source_file);
  let comments = SingleThreadedComments::default();

  let handler = Handler::with_emitter_and_flags(
    Box::new(error_buffer.clone()),
    HandlerFlags {
      can_emit_warnings: true,
      dont_buffer_diagnostics: true,
      ..HandlerFlags::default()
    },
  );

  let lexer = Lexer::new(syntax, TARGET, input, Some(&comments));
  let mut parser = swc_ecmascript::parser::Parser::new_from(lexer);

  let sm = &source_map;
  let module = parser.parse_module().map_err(move |err| {
    let mut diagnostic = err.into_diagnostic(&handler);
    diagnostic.emit();

    DiagnosticBuffer::from_error_buffer(error_buffer, |span| {
      sm.lookup_char_pos(span.lo)
    })
  })?;
  let leading_comments =
    comments.with_leading(module.span.lo, |comments| comments.to_vec());

  Ok(ParsedModule {
    leading_comments,
    module,
    source_map,
    comments,
    source_file,
  })
}

/// For a given specifier, source, and media type, parse the source of the
/// module and return a representation which can be further processed.
///
/// # Arguments
///
/// - `specifier` - The module specifier for the module.
/// - `source` - The source code for the module.
/// - `media_type` - The media type for the module.
///
// NOTE(bartlomieju): `specifier` has `&str` type instead of
// `&ModuleSpecifier` because runtime compiler APIs don't
// require valid module specifiers
pub fn parse(
  specifier: &str,
  source: &str,
  media_type: &MediaType,
) -> Result<ParsedModule, AnyError> {
  let source_map = Rc::new(SourceMap::default());
  parse_with_source_map(specifier, source, media_type, source_map)
}


/// A low level function which transpiles a source module into an swc
/// SourceFile.
pub fn transpile_module(
  filename: &str,
  src: &str,
  media_type: &MediaType,
  emit_options: &EmitOptions,
  globals: &Globals,
  cm: Rc<SourceMap>,
) -> Result<(Rc<SourceFile>, Module), AnyError> {
  let parsed_module =
    parse_with_source_map(filename, src, media_type, cm.clone())?;

  let jsx_pass = react::react(
    cm,
    Some(&parsed_module.comments),
    react::Options {
      pragma: emit_options.jsx_factory.clone(),
      pragma_frag: emit_options.jsx_fragment_factory.clone(),
      // this will use `Object.assign()` instead of the `_extends` helper
      // when spreading props.
      use_builtins: true,
      ..Default::default()
    },
  );
  let mut passes = chain!(
    Optional::new(jsx_pass, emit_options.transform_jsx),
    proposals::decorators::decorators(proposals::decorators::Config {
      legacy: true,
      emit_metadata: emit_options.emit_metadata
    }),
    helpers::inject_helpers(),
    typescript::strip(),
    fixer(Some(&parsed_module.comments)),
  );

  let source_file = parsed_module.source_file.clone();
  let module = parsed_module.module;

  let module = swc_common::GLOBALS.set(globals, || {
    helpers::HELPERS.set(&helpers::Helpers::new(false), || {
      module.fold_with(&mut passes)
    })
  });

  Ok((source_file, module))
}

pub struct BundleHook;

impl swc_bundler::Hook for BundleHook {
  fn get_import_meta_props(
    &self,
    span: swc_common::Span,
    module_record: &swc_bundler::ModuleRecord,
  ) -> Result<Vec<swc_ecmascript::ast::KeyValueProp>, AnyError> {
    use swc_ecmascript::ast;

    // we use custom file names, and swc "wraps" these in `<` and `>` so, we
    // want to strip those back out.
    let mut value = module_record.file_name.to_string();
    value.pop();
    value.remove(0);

    Ok(vec![
      ast::KeyValueProp {
        key: ast::PropName::Ident(ast::Ident::new("url".into(), span)),
        value: Box::new(ast::Expr::Lit(ast::Lit::Str(ast::Str {
          span,
          value: value.into(),
          kind: ast::StrKind::Synthesized,
          has_escape: false,
        }))),
      },
      ast::KeyValueProp {
        key: ast::PropName::Ident(ast::Ident::new("main".into(), span)),
        value: Box::new(if module_record.is_entry {
          ast::Expr::Member(ast::MemberExpr {
            span,
            obj: ast::ExprOrSuper::Expr(Box::new(ast::Expr::MetaProp(
              ast::MetaPropExpr {
                meta: ast::Ident::new("import".into(), span),
                prop: ast::Ident::new("meta".into(), span),
              },
            ))),
            prop: Box::new(ast::Expr::Ident(ast::Ident::new(
              "main".into(),
              span,
            ))),
            computed: false,
          })
        } else {
          ast::Expr::Lit(ast::Lit::Bool(ast::Bool { span, value: false }))
        }),
      },
    ])
  }
}
