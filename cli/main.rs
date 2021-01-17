// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

#![deny(warnings)]

#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;

mod ast;
mod checksum;
mod colors;
mod deno_dir;
mod diagnostics;

mod disk_cache;
mod errors;
mod file_fetcher;
mod flags;
mod fmt_errors;
mod fs_util;
mod http_cache;

mod import_map;
mod info;
mod lockfile;
mod media_type;
mod module_graph;
mod module_loader;
mod ops;
mod program_state;
mod source_maps;
mod specifier_handler;
mod text_encoding;
mod tokio_util;
mod version;

use crate::flags::DenoSubcommand;
use crate::flags::Flags;
use crate::fmt_errors::PrettyJsError;

use crate::module_loader::CliModuleLoader;
use crate::program_state::exit_unstable;
use crate::program_state::ProgramState;
use crate::source_maps::apply_source_map;

use deno_core::error::AnyError;
use deno_core::futures::future::FutureExt;
use deno_core::futures::Future;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::v8_set_flags;
use deno_core::ModuleSpecifier;

use deno_runtime::ops::worker_host::CreateWebWorkerCb;
use deno_runtime::permissions::Permissions;
use deno_runtime::web_worker::WebWorker;
use deno_runtime::web_worker::WebWorkerOptions;
use deno_runtime::worker::MainWorker;
use deno_runtime::worker::WorkerOptions;
use log::Level;
use log::LevelFilter;
use std::env;

use std::io::Write;
use std::iter::once;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;

fn create_web_worker_callback(
  program_state: Arc<ProgramState>,
) -> Arc<CreateWebWorkerCb> {
  Arc::new(move |args| {
    let global_state_ = program_state.clone();
    let js_error_create_fn = Rc::new(move |core_js_error| {
      let source_mapped_error =
        apply_source_map(&core_js_error, global_state_.clone());
      PrettyJsError::create(source_mapped_error)
    });

    let attach_inspector = program_state.maybe_inspector_server.is_some();
    let maybe_inspector_server = program_state.maybe_inspector_server.clone();

    let module_loader = CliModuleLoader::new_for_worker(
      program_state.clone(),
      args.parent_permissions.clone(),
    );
    let create_web_worker_cb =
      create_web_worker_callback(program_state.clone());

    let options = WebWorkerOptions {
      args: program_state.flags.argv.clone(),
      apply_source_maps: true,
      debug_flag: program_state
        .flags
        .log_level
        .map_or(false, |l| l == log::Level::Debug),
      unstable: program_state.flags.unstable,
      ca_data: program_state.ca_data.clone(),
      user_agent: version::get_user_agent(),
      seed: program_state.flags.seed,
      module_loader,
      create_web_worker_cb,
      js_error_create_fn: Some(js_error_create_fn),
      use_deno_namespace: args.use_deno_namespace,
      attach_inspector,
      maybe_inspector_server,
      runtime_version: version::deno(),
      ts_version: version::TYPESCRIPT.to_string(),
      no_color: !colors::use_color(),
      get_error_class_fn: Some(&crate::errors::get_error_class_name),
    };

    let mut worker = WebWorker::from_options(
      args.name,
      args.permissions,
      args.main_module,
      args.worker_id,
      &options,
    );

    // This block registers additional ops and state that
    // are only available in the CLI
    {
      let js_runtime = &mut worker.js_runtime;
      js_runtime
        .op_state()
        .borrow_mut()
        .put::<Arc<ProgramState>>(program_state.clone());
      // Applies source maps - works in conjuction with `js_error_create_fn`
      // above
      ops::errors::init(js_runtime);
    }
    worker.bootstrap(&options);

    worker
  })
}

pub fn create_main_worker(
  program_state: &Arc<ProgramState>,
  main_module: ModuleSpecifier,
  permissions: Permissions,
) -> MainWorker {
  let module_loader = CliModuleLoader::new(program_state.clone());

  let global_state_ = program_state.clone();

  let js_error_create_fn = Rc::new(move |core_js_error| {
    let source_mapped_error =
      apply_source_map(&core_js_error, global_state_.clone());
    PrettyJsError::create(source_mapped_error)
  });

  let attach_inspector = program_state.maybe_inspector_server.is_some()
    || program_state.flags.repl;
  let maybe_inspector_server = program_state.maybe_inspector_server.clone();
  let should_break_on_first_statement =
    program_state.flags.inspect_brk.is_some();

  let create_web_worker_cb = create_web_worker_callback(program_state.clone());

  let options = WorkerOptions {
    apply_source_maps: true,
    args: program_state.flags.argv.clone(),
    debug_flag: program_state
      .flags
      .log_level
      .map_or(false, |l| l == log::Level::Debug),
    unstable: program_state.flags.unstable,
    ca_data: program_state.ca_data.clone(),
    user_agent: version::get_user_agent(),
    seed: program_state.flags.seed,
    js_error_create_fn: Some(js_error_create_fn),
    create_web_worker_cb,
    attach_inspector,
    maybe_inspector_server,
    should_break_on_first_statement,
    module_loader,
    runtime_version: version::deno(),
    ts_version: version::TYPESCRIPT.to_string(),
    no_color: !colors::use_color(),
    get_error_class_fn: Some(&crate::errors::get_error_class_name),
    location: program_state.flags.location.clone(),
  };

  let mut worker = MainWorker::from_options(main_module, permissions, &options);

  // This block registers additional ops and state that
  // are only available in the CLI
  {
    let js_runtime = &mut worker.js_runtime;
    js_runtime
      .op_state()
      .borrow_mut()
      .put::<Arc<ProgramState>>(program_state.clone());
    // Applies source maps - works in conjuction with `js_error_create_fn`
    // above
    ops::errors::init(js_runtime);
  }
  worker.bootstrap(&options);

  worker
}

fn write_to_stdout_ignore_sigpipe(bytes: &[u8]) -> Result<(), std::io::Error> {
  use std::io::ErrorKind;

  match std::io::stdout().write_all(bytes) {
    Ok(()) => Ok(()),
    Err(e) => match e.kind() {
      ErrorKind::BrokenPipe => Ok(()),
      _ => Err(e),
    },
  }
}

fn write_json_to_stdout<T>(value: &T) -> Result<(), AnyError>
where
  T: ?Sized + serde::ser::Serialize,
{
  let writer = std::io::BufWriter::new(std::io::stdout());
  serde_json::to_writer_pretty(writer, value).map_err(AnyError::from)
}

fn print_cache_info(
  state: &Arc<ProgramState>,
  json: bool,
) -> Result<(), AnyError> {
  let deno_dir = &state.dir.root;
  let modules_cache = &state.file_fetcher.get_http_cache_location();
  let typescript_cache = &state.dir.gen_cache.location;
  if json {
    let output = json!({
        "denoDir": deno_dir,
        "modulesCache": modules_cache,
        "typescriptCache": typescript_cache,
    });
    write_json_to_stdout(&output)
  } else {
    println!("{} {:?}", colors::bold("DENO_DIR location:"), deno_dir);
    println!(
      "{} {:?}",
      colors::bold("Remote modules cache:"),
      modules_cache
    );
    println!(
      "{} {:?}",
      colors::bold("TypeScript compiler cache:"),
      typescript_cache
    );
    Ok(())
  }
}

async fn info_command(
  flags: Flags,
  maybe_specifier: Option<String>,
  json: bool,
) -> Result<(), AnyError> {
  if json && !flags.unstable {
    exit_unstable("--json");
  }
  let program_state = ProgramState::new(flags)?;
  if let Some(specifier) = maybe_specifier {
    let specifier = ModuleSpecifier::resolve_url_or_path(&specifier)?;
    let handler = Arc::new(Mutex::new(specifier_handler::FetchHandler::new(
      &program_state,
      // info accesses dynamically imported modules just for their information
      // so we allow access to all of them.
      Permissions::allow_all(),
    )?));
    let mut builder = module_graph::GraphBuilder::new(
      handler,
      program_state.maybe_import_map.clone(),
      program_state.lockfile.clone(),
    );
    builder.add(&specifier, false).await?;
    let graph = builder.get_graph();
    let info = graph.info()?;

    if json {
      write_json_to_stdout(&json!(info))?;
    } else {
      write_to_stdout_ignore_sigpipe(info.to_string().as_bytes())?;
    }
    Ok(())
  } else {
    // If it was just "deno info" print location of caches and exit
    print_cache_info(&program_state, json)
  }
}


pub async fn run_command(flags: Flags, script: String) -> Result<(), AnyError> {

  let main_module = ModuleSpecifier::resolve_url_or_path(&script)?;
  let program_state = ProgramState::new(flags.clone())?;
  let permissions = Permissions::from_options(&flags.clone().into());
  let mut worker =
    create_main_worker(&program_state, main_module.clone(), permissions);

  debug!("main_module {}", main_module);
  worker.execute_module(&main_module).await?;
  // Hint: 这个时候实际上同步的代码已经执行完成了
  worker.execute("window.dispatchEvent(new Event('load'))")?;
  worker.run_event_loop().await?;
  worker.execute("window.dispatchEvent(new Event('unload'))")?;


  Ok(())
}

fn init_v8_flags(v8_flags: &[String]) {
  let v8_flags_includes_help = v8_flags
    .iter()
    .any(|flag| flag == "-help" || flag == "--help");
  // Keep in sync with `standalone.rs`.
  let v8_flags = once("UNUSED_BUT_NECESSARY_ARG0".to_owned())
    .chain(v8_flags.iter().cloned())
    .collect::<Vec<_>>();
  let unrecognized_v8_flags = v8_set_flags(v8_flags)
    .into_iter()
    .skip(1)
    .collect::<Vec<_>>();
  if !unrecognized_v8_flags.is_empty() {
    for f in unrecognized_v8_flags {
      eprintln!("error: V8 did not recognize flag '{}'", f);
    }
    eprintln!("\nFor a list of V8 flags, use '--v8-flags=--help'");
    std::process::exit(1);
  }
  if v8_flags_includes_help {
    std::process::exit(0);
  }
}

fn init_logger(maybe_level: Option<Level>) {
  let log_level = match maybe_level {
    Some(level) => level,
    None => Level::Info, // Default log level
  };
  env_logger::Builder::from_env(
    env_logger::Env::default()
      .default_filter_or(log_level.to_level_filter().to_string()),
  )
  // https://github.com/denoland/deno/issues/6641
  .filter_module("rustyline", LevelFilter::Off)
  .format(|buf, record| {
    let mut target = record.target().to_string();
    if let Some(line_no) = record.line() {
      target.push(':');
      target.push_str(&line_no.to_string());
    }
    if record.level() <= Level::Info {
      // Print ERROR, WARN, INFO logs as they are
      writeln!(buf, "{}", record.args())
    } else {
      // Add prefix to DEBUG or TRACE logs
      writeln!(
        buf,
        "{} RS - {} - {}",
        record.level(),
        target,
        record.args()
      )
    }
  })
  .init();
}

fn get_subcommand(
  flags: Flags,
) -> Pin<Box<dyn Future<Output = Result<(), AnyError>>>> {
  debug!("flags.clone().subcommand : {:?}", flags.clone().subcommand );
  match flags.clone().subcommand {
    DenoSubcommand::Info { file, json } => {
      info_command(flags, file, json).boxed_local()
    }
    DenoSubcommand::Run { script } => run_command(flags, script).boxed_local(),
    _ => {
      unimplemented!("not support")
    }
  }
}

fn unwrap_or_exit<T>(result: Result<T, AnyError>) -> T {
  match result {
    Ok(value) => value,
    Err(error) => {
      let msg = format!(
        "{}: {}",
        colors::red_bold("error"),
        error.to_string().trim()
      );
      eprintln!("{}", msg);
      std::process::exit(1);
    }
  }
}

pub fn main() {
  #[cfg(windows)]
  colors::enable_ansi(); // For Windows 10

  println!("deno run:{:?}", std::process::id());

  let args: Vec<String> = env::args().collect();

  let flags = match flags::flags_from_vec(args) {
    Ok(flags) => flags,
    Err(err @ clap::Error { .. })
      if err.kind == clap::ErrorKind::HelpDisplayed
        || err.kind == clap::ErrorKind::VersionDisplayed =>
    {
      err.write_to(&mut std::io::stdout()).unwrap();
      std::process::exit(0);
    }
    Err(err) => unwrap_or_exit(Err(AnyError::from(err))),
  };
  if !flags.v8_flags.is_empty() {
    init_v8_flags(&*flags.v8_flags);
  }
  debug!("flags.log_level: {:?}", flags.log_level);

  init_logger(flags.log_level);

  // 指定了 log-level 的情况下去休眠，方便我们 attach
  // ./deno --log-level debug run main.ts
  if flags.log_level.is_some() {
    std::thread::sleep(std::time::Duration::from_millis(20_000));
  }

  unwrap_or_exit(tokio_util::run_basic(get_subcommand(flags)));
}
