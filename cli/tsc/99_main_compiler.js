// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

// deno-lint-ignore-file no-undef

// This module is the entry point for "compiler" isolate, ie. the one
// that is created when Deno needs to type check TypeScript, and in some
// instances convert TypeScript to JavaScript.

// Removes the `__proto__` for security reasons.  This intentionally makes
// Deno non compliant with ECMA-262 Annex B.2.2.1
delete Object.prototype.__proto__;

((window) => {
  /** @type {DenoCore} */
  const core = window.Deno.core;

  let logDebug = false;
  let logSource = "JS";

  function setLogDebug(debug, source) {
    logDebug = debug;
    if (source) {
      logSource = source;
    }
  }

  function debug(...args) {
    if (logDebug) {
      const stringifiedArgs = args.map((arg) =>
        typeof arg === "string" ? arg : JSON.stringify(arg)
      ).join(" ");
      // adding a non-zero integer value to the end of the debug string causes
      // the message to be printed to stderr instead of stdout, which is better
      // aligned to the behaviour of debug messages
      core.print(`DEBUG ${logSource} - ${stringifiedArgs}\n`, 1);
    }
  }


  // exposes the two functions that are called by `tsc::exec()` when type
  // checking TypeScript.

  /** Startup the runtime environment, setting various flags.
   * @param {{ debugFlag?: boolean; legacyFlag?: boolean; }} msg
   */
  function startup({ debugFlag = false }) {
    if (hasStarted) {
      throw new Error("The compiler runtime already started.");
    }
    hasStarted = true;
    setLogDebug(!!debugFlag, "TS");
  }

  /** @param {{ debug: boolean; }} init */
  function serverInit({ debug: debugFlag }) {
    if (hasStarted) {
      throw new Error("The language server has already been initialized.");
    }
    hasStarted = true;
    core.ops();
    setLogDebug(debugFlag, "TSLS");
    debug("serverInit()");
  }

  let hasStarted = false;

  /**
   * @param {LanguageServerRequest} request
   */
  function serverRequest({ id, ...request }) {
    debug(`serverRequest()`, { id, ...request });
  }

  globalThis.startup = startup;


  // exposes the functions that are called when the compiler is used as a
  // language service.
  globalThis.serverInit = serverInit;
  globalThis.serverRequest = serverRequest;
})(this);
