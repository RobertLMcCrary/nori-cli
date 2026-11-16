use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::*;
use crate::engine::ScriptEngine;
use crate::error::Origin;
use crate::error::ScriptError;

fn denied_at(source: &str, needle: &str, message: &str) {
    let offset = source.find(needle).expect("needle is in the source");
    assert_eq!(
        check(source),
        Err(Failure::new(ErrorKind::Sandbox, Some(offset), message)),
        "{source}"
    );
}

#[test]
fn plain_dsl_and_pure_steel_pass() {
    let source = r#"
; a comment mentioning display and (require "x")
(define (twice n) (* 2 n))
(define (strict w) (~> w (turns 1) (on-stop "Run the tests.")))
(define message (cond [(> (twice 2) 3) "big"] [else "small"]))
(define-workflow land
  (turns (twice 2))
  (pr #:sections (map (lambda (s) (string-append s ":")) '("Why" "What"))))
"#;
    assert_eq!(check(source), Ok(()));
}

#[test]
fn files_processes_environment_and_terminal_are_denied() {
    denied_at(
        r#"(open-input-file "/etc/passwd")"#,
        "open-input-file",
        "`open-input-file` is not available: scripts may not touch files or the terminal",
    );
    denied_at(
        r#"(define x 1) (command "rm" '("-rf" "/"))"#,
        "command",
        "`command` is not available: scripts may not run processes",
    );
    denied_at(
        r#"(env-var "HOME")"#,
        "env-var",
        "`env-var` is not available: scripts may not read or change the environment",
    );
    denied_at(
        r#"(displayln "hi")"#,
        "displayln",
        "`displayln` is not available: scripts may not touch files or the terminal",
    );
    denied_at(
        "(read-line)",
        "read-line",
        "`read-line` is not available: scripts may not touch files or the terminal",
    );
    denied_at(
        "(time/sleep-ms 100000)",
        "time/sleep-ms",
        "`time/sleep-ms` is not available: scripts may not start threads, block, or sleep",
    );
}

#[test]
fn loading_and_evaluating_code_is_denied() {
    denied_at(
        "(eval '(+ 1 2))",
        "eval",
        "`eval` is not available: scripts may not evaluate or load other code",
    );
    denied_at(
        "(require-builtin steel/process)",
        "require-builtin",
        "`require-builtin` is not available: scripts may not evaluate or load other code",
    );
    denied_at(
        r#"(require "../secrets.scm")"#,
        "require",
        "`require` is not available: nori/workflow is already loaded, and scripts may not load other files or modules",
    );
    denied_at(
        "((Engine::new))",
        "Engine::new",
        "`Engine::new` is not available in Nori scripts",
    );
    denied_at(
        "(#%prim.command \"ls\")",
        "#%prim.command",
        "`#%prim.command` is a Steel internal; scripts may not use it",
    );
    denied_at(
        "(%module-get% x 'y)",
        "%module-get%",
        "`%module-get%` is a Steel internal; scripts may not use it",
    );
}

#[test]
fn macro_definitions_are_denied() {
    let message = "scripts may not define macros, whose expansion cannot be interrupted; use a function such as (define (strict w) (~> w (turns 1)))";
    denied_at(
        "(define-syntax inf (syntax-rules () [(inf x) (inf (x x))]))",
        "define-syntax",
        message,
    );
    denied_at("(define-syntax (m stx) stx)", "define-syntax", message);
    denied_at(
        "(defmacro (m stx) stx)",
        "defmacro",
        "`defmacro` is not available: scripts may not evaluate or load other code",
    );
}

#[test]
fn malformed_sources_are_syntax_errors() {
    assert_eq!(
        check("(turns 3))"),
        Err(Failure::new(ErrorKind::Syntax, Some(9), "unexpected `)`"))
    );
    assert_eq!(
        check("(a (b)"),
        Err(Failure::new(
            ErrorKind::Syntax,
            Some(0),
            "this form is never closed"
        ))
    );
    let unterminated = check(r#"(on-stop "never closed)"#).expect_err("rejected");
    assert_eq!(unterminated.kind, ErrorKind::Syntax);
}

#[test]
fn size_and_depth_are_bounded() {
    let deep = format!("{}{}", "(".repeat(65), ")".repeat(65));
    assert_eq!(
        check(&deep),
        Err(Failure::new(
            ErrorKind::Sandbox,
            Some(64),
            "forms may nest at most 64 levels deep"
        ))
    );
    let huge = format!("; {}", "x".repeat(MAX_SOURCE_BYTES));
    assert_eq!(
        check(&huge),
        Err(Failure::new(
            ErrorKind::Sandbox,
            Some(0),
            "scripts are limited to 256 KiB"
        ))
    );
}

#[test]
fn a_runaway_loop_is_stopped() {
    let mut engine = ScriptEngine::new().expect("engine");
    let started = std::time::Instant::now();
    let source = "(define (spin n) (spin (+ n 1)))\n(spin 0)";
    let error = engine
        .run(source)
        .map_err(|failure| failure.locate(Origin::File(PathBuf::from("loop.scm")), source))
        .expect_err("interrupted");
    assert_eq!(
        error,
        ScriptError {
            origin: Origin::File(PathBuf::from("loop.scm")),
            kind: ErrorKind::Timeout,
            line: None,
            column: None,
            message: "evaluation ran longer than 500 ms and was stopped".to_string(),
        }
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    // The engine is usable again afterwards.
    assert_eq!(engine.run("(+ 1 2)").map(|values| values.len()), Ok(1));
}
