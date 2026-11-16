//! What a Nori script may not touch, and the static check that enforces it
//! before Steel sees the source.
//!
//! The sandbox has four layers, because Steel's own `Engine::new_sandboxed`
//! still binds process, environment, terminal, and `eval` builtins:
//!
//! 1. [`check`] scans the source with Steel's own lexer, so it agrees with the
//!    compiler about what is an identifier. It rejects denied names, `require`,
//!    macro definitions, Steel internals, oversized sources, and deep nesting.
//!    Without `eval` or user macros, code cannot name a global that does not
//!    appear literally in its source, so this layer is sufficient alone.
//!    Macros are denied because Steel expands them in the compiler, where the
//!    watchdog cannot reach: a self-referential `syntax-rules` macro hangs or
//!    overflows the stack.
//! 2. The engine rebinds every denied global to a function that raises an
//!    error (`engine.rs`), in case a name reaches the compiler anyway.
//! 3. The engine replaces the dangerous builtin modules with empty ones, so a
//!    smuggled `require-builtin` binds nothing.
//! 4. A watchdog interrupts evaluation that runs past its time budget, and a
//!    panic inside Steel becomes an error.

use steel::parser::lexer::TokenStream;
use steel::parser::tokens::TokenType;

use crate::error::ErrorKind;
use crate::error::Failure;

/// Larger scripts are rejected before parsing.
pub(crate) const MAX_SOURCE_BYTES: usize = 256 * 1024;

/// Deeper nesting is rejected before parsing; Steel's parser recurses.
pub(crate) const MAX_DEPTH: i64 = 64;

/// Builtin modules a script must never load, even through a smuggled
/// `require-builtin`.
pub(crate) const DENIED_MODULES: &[&str] = &[
    "steel/filesystem",
    "steel/io",
    "steel/meta",
    "steel/ports",
    "steel/process",
    "steel/threads",
    "steel/tcp",
    "steel/http",
    "steel/polling",
    "steel/git",
    "steel/ffi",
];

/// Evaluating new code, inspecting the runtime, or deferring code past the
/// time budget.
const EVAL: &[&str] = &[
    "eval",
    "eval!",
    "eval-string",
    "expand!",
    "load",
    "load-expanded",
    "load-from-module!",
    "emit-expanded",
    "run!",
    "read",
    "read!",
    "read-syntax-object",
    "require-builtin",
    "defmacro",
    "syntax-case",
    "let-syntax",
    "letrec-syntax",
    "register-macro-transformer!",
    "debug-globals",
    "inspect",
    "breakpoint!",
    "set-test-mode!",
    "get-test-mode",
    "dump-profiler",
    "make-callstack-profiler",
    "callstack-hydrate-names",
    "module->exports",
    "current-module",
    "current-module-relative",
    "memory-address",
    "active-object-count",
    "serialize-value",
    "deserialize-value",
    "serialized->bytes",
    "bytes->serialized",
    "with-finalizer",
    "will-execute",
    "will-register",
    "make-will-executor",
];

const PROCESS: &[&str] = &[
    "command",
    "command-builder?",
    "ChildProcess?",
    "CommandBuilder?",
    "child-stderr",
    "child-stdin",
    "child-stdout",
    "kill",
    "process-wait",
    "spawn-process",
    "subprocess-kill",
    "subprocess?",
    "wait",
    "wait->stdout",
    "which",
    "set-piped-stdout!",
    "set-stdout-piped!",
    "with-stdout-piped",
    "with-stderr-piped",
    "with-stdin-piped",
    "with-stdin",
    "with-stdout",
    "with-stderr",
];

const ENVIRONMENT: &[&str] = &[
    "env-var",
    "maybe-get-env-var",
    "set-env-var!",
    "with-env-var",
    "without-env-var",
    "with-cleared-env-vars",
    "set-current-dir!",
    "with-current-dir",
    "command-line",
    "steel-home-location",
];

/// Files and the terminal Nori is drawing on.
const FILES_AND_TERMINAL: &[&str] = &[
    "open-input-file",
    "open-output-file",
    "call-with-input-file",
    "call-with-output-file",
    "with-input-from-file",
    "with-output-to-file",
    "stdin",
    "stdout",
    "current-input-port",
    "current-output-port",
    "current-error-port",
    "read-line",
    "read-char",
    "peek-char",
    "read-byte",
    "peek-byte",
    "read-u8",
    "peek-u8",
    "read-bytes",
    "read-bytes!",
    "read-bytevector",
    "read-bytevector!",
    "read-bytes-into-buf",
    "read-line-from-port",
    "read-port-to-bytes",
    "read-port-to-string",
    "read-to-string",
    "port->string",
    "port->bytes",
    "display",
    "displayln",
    "newline",
    "print",
    "println",
    "simple-display",
    "simple-displayln",
    "stdout-simple-displayln",
    "write",
    "write-char",
    "write-string",
    "write-line!",
    "write-byte",
    "write-bytes",
    "write-u8",
    "write-bytevector",
    "flush-output-port",
];

/// Threads, blocking, and sleeping, which a watchdog cannot interrupt.
const THREADS: &[&str] = &[
    "spawn-native-thread",
    "thread-join!",
    "thread-interrupt",
    "thread-resume",
    "thread-suspend",
    "thread-finished?",
    "thread/available-parallelism",
    "current-thread-id",
    "channel/recv",
    "channel/send",
    "channel/try-recv",
    "channels-receiver",
    "channels-sender",
    "channels/new",
    "receivers-select",
    "mutex",
    "lock-acquire!",
    "lock-release!",
    "make-tls",
    "get-tls",
    "set-tls!",
    "time/sleep-ms",
    "block-on",
    "local-executor/block-on",
    "poll!",
    "join!",
    "futures-join-all",
];

/// Every denied global, for the engine to rebind.
pub(crate) fn denied_globals() -> impl Iterator<Item = &'static str> {
    [EVAL, PROCESS, ENVIRONMENT, FILES_AND_TERMINAL, THREADS]
        .into_iter()
        .flatten()
        .copied()
}

/// Why `name` may not be used, or `None` when it may.
pub(crate) fn denial(name: &str) -> Option<String> {
    if name.starts_with('%') || name.starts_with("#%") || name.contains("__%#__") {
        return Some(format!(
            "`{name}` is a Steel internal; scripts may not use it"
        ));
    }
    if name.contains("::") {
        return Some(format!("`{name}` is not available in Nori scripts"));
    }
    let reason = if EVAL.contains(&name) {
        "scripts may not evaluate or load other code"
    } else if PROCESS.contains(&name) {
        "scripts may not run processes"
    } else if ENVIRONMENT.contains(&name) {
        "scripts may not read or change the environment"
    } else if FILES_AND_TERMINAL.contains(&name) {
        "scripts may not touch files or the terminal"
    } else if THREADS.contains(&name) {
        "scripts may not start threads, block, or sleep"
    } else {
        return None;
    };
    Some(format!("`{name}` is not available: {reason}"))
}

/// Rejects a source that is malformed or reaches outside the sandbox.
pub(crate) fn check(source: &str) -> Result<(), Failure> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(Failure::new(
            ErrorKind::Sandbox,
            Some(0),
            format!("scripts are limited to {} KiB", MAX_SOURCE_BYTES / 1024),
        ));
    }
    let mut tokens = Vec::new();
    for token in TokenStream::new(source, true, None) {
        match token {
            Ok(token) => tokens.push((token.ty, token.span.start as usize)),
            Err(error) => {
                return Err(Failure::new(
                    ErrorKind::Syntax,
                    Some(error.span.start as usize),
                    error.ty.to_string(),
                ));
            }
        }
    }
    let mut open: Vec<usize> = Vec::new();
    for (ty, offset) in &tokens {
        let offset = *offset;
        match ty {
            TokenType::OpenParen(..) => {
                open.push(offset);
                if i64::try_from(open.len()).unwrap_or(i64::MAX) > MAX_DEPTH {
                    return Err(Failure::new(
                        ErrorKind::Sandbox,
                        Some(offset),
                        format!("forms may nest at most {MAX_DEPTH} levels deep"),
                    ));
                }
            }
            TokenType::CloseParen(paren) => {
                if open.pop().is_none() {
                    let close = paren.close();
                    return Err(Failure::new(
                        ErrorKind::Syntax,
                        Some(offset),
                        format!("unexpected `{close}`"),
                    ));
                }
            }
            TokenType::Require => {
                return Err(Failure::new(
                    ErrorKind::Sandbox,
                    Some(offset),
                    "`require` is not available: nori/workflow is already loaded, and scripts may not load other files or modules",
                ));
            }
            TokenType::DefineSyntax | TokenType::SyntaxRules => {
                return Err(Failure::new(
                    ErrorKind::Sandbox,
                    Some(offset),
                    "scripts may not define macros, whose expansion cannot be interrupted; use a function such as (define (strict w) (~> w (turns 1)))",
                ));
            }
            TokenType::Identifier(name) => {
                if let Some(message) = denial(name.resolve()) {
                    return Err(Failure::new(ErrorKind::Sandbox, Some(offset), message));
                }
            }
            TokenType::QuoteTick
            | TokenType::QuasiQuote
            | TokenType::Unquote
            | TokenType::UnquoteSplice
            | TokenType::QuoteSyntax
            | TokenType::QuasiQuoteSyntax
            | TokenType::UnquoteSyntax
            | TokenType::UnquoteSpliceSyntax
            | TokenType::If
            | TokenType::Define
            | TokenType::Let
            | TokenType::TestLet
            | TokenType::Return
            | TokenType::Begin
            | TokenType::Lambda
            | TokenType::Quote
            | TokenType::Ellipses
            | TokenType::Set
            | TokenType::CharacterLiteral(_)
            | TokenType::DatumComment
            | TokenType::Comment
            | TokenType::BooleanLiteral(_)
            | TokenType::Keyword(_)
            | TokenType::Number(_)
            | TokenType::StringLiteral(_)
            | TokenType::Dot => {}
        }
    }
    if let Some(offset) = open.pop() {
        return Err(Failure::new(
            ErrorKind::Syntax,
            Some(offset),
            "this form is never closed",
        ));
    }
    Ok(())
}

/// Forms that define or register something, which inline DSL may not use: it
/// adjusts one prompt's workflow and leaves the loaded definitions alone.
const DEFINING: &[&str] = &["define-workflow", "register-workflow", "default-workflow"];

/// [`check`], and also rejects definitions: for inline DSL, which holds clauses
/// only.
pub(crate) fn check_clauses(source: &str) -> Result<(), Failure> {
    check(source)?;
    for token in TokenStream::new(source, true, None).flatten() {
        let defines = matches!(token.ty, TokenType::Define | TokenType::Set)
            || matches!(&token.ty, TokenType::Identifier(name) if DEFINING.contains(&name.resolve()));
        if defines {
            return Err(Failure::new(
                ErrorKind::Sandbox,
                Some(token.span.start as usize),
                "inline DSL takes clauses such as (turns 3); put definitions in init.scm or .nori/nori.scm",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
