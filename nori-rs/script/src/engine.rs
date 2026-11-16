//! A sandboxed Steel engine with `nori/workflow` loaded.
//!
//! Steel engines are not `Send`, so one is created, used, and dropped on a
//! single thread. Nothing in this module outlives a call into the crate.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;

use steel::SteelErr;
use steel::SteelVal;
use steel::steel_vm::builtin::BuiltInModule;
use steel::steel_vm::engine::Engine;

use crate::dsl;
use crate::dsl::Registry;
use crate::dsl::SharedRegistry;
use crate::error::ErrorKind;
use crate::error::Failure;
use crate::sandbox;

/// The longest one file or one inline chunk may run.
pub const TIME_BUDGET: Duration = Duration::from_millis(500);

pub(crate) struct ScriptEngine {
    engine: Engine,
    registry: SharedRegistry,
}

impl ScriptEngine {
    pub(crate) fn new() -> Result<Self, Failure> {
        let created = catch_unwind(|| {
            let registry = SharedRegistry::default();
            let mut engine = Engine::new_sandboxed();
            for module in sandbox::DENIED_MODULES {
                engine.register_module(BuiltInModule::new(*module));
            }
            for name in sandbox::denied_globals() {
                let stub = denied_stub(name);
                // Unshadowed references to builtins compile to their
                // `#%prim.` alias, so rebind both names.
                let reserved = format!("#%prim.{name}");
                if engine.global_exists(&reserved) {
                    engine.register_value(&reserved, stub.clone());
                }
                engine.register_value(name, stub);
            }
            engine.register_module(dsl::module(&registry));
            engine
                .run(dsl::PRELUDE)
                .map(|_| Self { engine, registry })
                .map_err(|error| error.to_string())
        });
        match created {
            Ok(Ok(engine)) => Ok(engine),
            Ok(Err(message)) => Err(Failure::new(
                ErrorKind::Eval,
                None,
                format!("could not start the script engine: {message}"),
            )),
            Err(panic) => Err(crashed(panic.as_ref())),
        }
    }

    /// Checks and runs `source`, returning its top-level values.
    ///
    /// Steel is never given the script's path: with one it reads the file
    /// itself. Errors are located against `source` instead.
    pub(crate) fn run(&mut self, source: &str) -> Result<Vec<SteelVal>, Failure> {
        sandbox::check(source)?;
        let controller = self.engine.get_thread_state_controller();
        let watchdog_controller = controller.clone();
        let (finished, finished_signal) = mpsc::channel::<()>();
        let watchdog = thread::Builder::new()
            .name("nori-script-watchdog".to_string())
            .spawn(move || {
                let expired = matches!(
                    finished_signal.recv_timeout(TIME_BUDGET),
                    Err(RecvTimeoutError::Timeout)
                );
                if expired {
                    watchdog_controller.interrupt();
                }
                expired
            })
            .map_err(|error| {
                Failure::new(
                    ErrorKind::Eval,
                    None,
                    format!("could not start the script watchdog: {error}"),
                )
            })?;
        let engine = &mut self.engine;
        let program = source.to_string();
        let result = catch_unwind(AssertUnwindSafe(|| {
            engine.compile_and_run_raw_program(program)
        }));
        drop(finished);
        let expired = watchdog.join().unwrap_or(true);
        if expired {
            controller.resume();
        }
        match result {
            Ok(Ok(values)) => Ok(values),
            Ok(Err(_)) if expired => Err(Failure::new(
                ErrorKind::Timeout,
                None,
                format!(
                    "evaluation ran longer than {} ms and was stopped",
                    TIME_BUDGET.as_millis()
                ),
            )),
            Ok(Err(error)) => Err(self.failure(&error, source)),
            Err(panic) => Err(crashed(panic.as_ref())),
        }
    }

    /// Binds `name` to `value` for later evaluations.
    pub(crate) fn bind(&mut self, name: &str, value: SteelVal) {
        self.engine.register_value(name, value);
    }

    pub(crate) fn registry(&self) -> Registry {
        dsl::snapshot(&self.registry)
    }

    /// Puts the registry back as it was in `snapshot`.
    pub(crate) fn restore(&mut self, snapshot: Registry) {
        dsl::replace(&self.registry, snapshot);
    }

    /// `error` with its message cleaned up and its position in `source`, when
    /// Steel reported one there.
    fn failure(&self, error: &SteelErr, source: &str) -> Failure {
        let kind = error.kind();
        let rendered = error.to_string();
        let message = rendered
            .strip_prefix(&format!("Error: {kind:?}: "))
            .unwrap_or(&rendered)
            .trim()
            .to_string();
        let in_source = |span: &steel::parser::span::Span| {
            span.source_id
                .and_then(|id| self.engine.get_source(&id))
                .is_some_and(|text| **text == *source)
        };
        let offset = error
            .span()
            .into_iter()
            .chain(
                error
                    .stack_trace()
                    .iter()
                    .flat_map(|trace| trace.trace().iter().filter_map(|context| *context.span())),
            )
            .find(in_source)
            .map(|span| span.start as usize);
        let message = match kind {
            steel::rerrs::ErrorKind::FreeIdentifier => {
                format!("{message} (not a Steel builtin or a nori/workflow form)")
            }
            steel::rerrs::ErrorKind::Parse => message,
            steel::rerrs::ErrorKind::ArityMismatch
            | steel::rerrs::ErrorKind::TypeMismatch
            | steel::rerrs::ErrorKind::UnexpectedToken
            | steel::rerrs::ErrorKind::ContractViolation
            | steel::rerrs::ErrorKind::BadSyntax
            | steel::rerrs::ErrorKind::ConversionError
            | steel::rerrs::ErrorKind::Io
            | steel::rerrs::ErrorKind::Infallible
            | steel::rerrs::ErrorKind::Generic => message,
        };
        let kind = match kind {
            steel::rerrs::ErrorKind::Parse | steel::rerrs::ErrorKind::UnexpectedToken => {
                ErrorKind::Syntax
            }
            steel::rerrs::ErrorKind::ArityMismatch
            | steel::rerrs::ErrorKind::FreeIdentifier
            | steel::rerrs::ErrorKind::TypeMismatch
            | steel::rerrs::ErrorKind::ContractViolation
            | steel::rerrs::ErrorKind::BadSyntax
            | steel::rerrs::ErrorKind::ConversionError
            | steel::rerrs::ErrorKind::Io
            | steel::rerrs::ErrorKind::Infallible
            | steel::rerrs::ErrorKind::Generic => ErrorKind::Eval,
        };
        Failure::new(kind, offset, message)
    }
}

/// A function standing in for a denied builtin.
fn denied_stub(name: &'static str) -> SteelVal {
    SteelVal::anonymous_boxed_function(Arc::new(move |_args: &[SteelVal]| {
        let message = sandbox::denial(name).unwrap_or_else(|| format!("`{name}` is not available"));
        Err(SteelErr::new(steel::rerrs::ErrorKind::Generic, message))
    }))
}

fn crashed(panic: &(dyn Any + Send)) -> Failure {
    let detail = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("no details");
    Failure::new(
        ErrorKind::Eval,
        None,
        format!("the script engine failed internally: {detail}"),
    )
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
