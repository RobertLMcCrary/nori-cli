use pretty_assertions::assert_eq;

use super::*;

/// Runs `source` past the static check, as if it had slipped through.
fn run_unchecked(source: &str) -> Result<Vec<SteelVal>, String> {
    let mut engine = ScriptEngine::new().expect("engine");
    engine
        .engine
        .run(source.to_string())
        .map_err(|error| error.to_string())
}

#[test]
fn denied_globals_are_rebound_even_without_the_static_check() {
    for (source, name) in [
        (r#"(env-var "HOME")"#, "env-var"),
        (r#"(display "corrupts the TUI")"#, "display"),
        (r#"(command "ls" '())"#, "command"),
        (r#"(open-input-file "/etc/hosts")"#, "open-input-file"),
        ("(eval '(+ 1 2))", "eval"),
        ("(define f env-var) (f \"HOME\")", "env-var"),
    ] {
        let error = run_unchecked(source).expect_err(source);
        let expected = sandbox::denial(name).expect("denied");
        assert!(error.contains(&expected), "{source}: {error}");
    }
}

#[test]
fn denied_modules_are_empty() {
    let error = run_unchecked("(require-builtin steel/process) (spawn-process \"ls\" '())")
        .expect_err("no process access");
    assert!(error.contains("not available"), "{error}");
}

#[test]
fn every_global_is_reviewed() {
    let engine = ScriptEngine::new().expect("engine");
    let reviewed: Vec<&str> = include_str!("reviewed_globals.txt").lines().collect();
    let dsl_forms = [
        "workflow",
        "workflow?",
        "turns",
        "instruct",
        "commit-style",
        "commit-agent",
        "pr",
        "use",
        "register-workflow",
        "default-workflow",
        "on-change",
        "on-turn",
        "on-commit",
        "on-pr",
        "on-stop",
        "hook-after-change",
        "hook-after-turn",
        "hook-after-commit",
        "hook-before-pr",
        "hook-before-stop",
    ];
    let mut unreviewed: Vec<String> = engine
        .engine
        .readable_globals(0)
        .iter()
        .map(|name| name.resolve().to_string())
        .filter(|name| {
            sandbox::denial(name).is_none()
                && !reviewed.contains(&name.as_str())
                && !dsl_forms.contains(&name.as_str())
        })
        .collect();
    unreviewed.sort();
    unreviewed.dedup();
    // A Steel upgrade that adds globals fails here: deny each new name in
    // `sandbox.rs` or add it to `reviewed_globals.txt`.
    assert_eq!(unreviewed, Vec::<String>::new());
}
