use pretty_assertions::assert_eq;

use super::*;
use crate::model::CommitAgent;
use crate::model::CommitRules;
use crate::model::Hook;
use crate::model::HookEvent;

fn chunks(prompt: &str) -> Vec<(&str, usize)> {
    split(prompt)
        .1
        .into_iter()
        .map(|chunk| (chunk.text, chunk.offset))
        .collect()
}

#[test]
fn a_prompt_without_dsl_is_untouched() {
    let prompt = "Fix the bug in `parse(x)`,\n  ,(not dsl) because it is indented\n";
    assert_eq!(split(prompt), (prompt.to_string(), Vec::new()));
    assert!(!has_inline_dsl(prompt));
}

#[test]
fn comma_lines_at_column_zero_are_dsl() {
    let prompt = "Fix the flaky test.\n,(turns 2)\nThen tidy up.\n,(on-stop \"Run it twice.\")";
    assert_eq!(split(prompt).0, "Fix the flaky test.\nThen tidy up.");
    assert_eq!(
        chunks(prompt),
        vec![("(turns 2)", 21), ("(on-stop \"Run it twice.\")", 46)]
    );
    assert!(has_inline_dsl(prompt));
}

#[test]
fn nori_fences_are_dsl_and_other_fences_are_not() {
    let prompt = "\
Ship it.
```nori
(turns 1)
(pr #:draft #t)
```
```scheme
,(not dsl inside a scheme block)
```
~~~nori
(on-stop \"Done?\")
~~~
";
    assert_eq!(
        split(prompt).0,
        "Ship it.\n```scheme\n,(not dsl inside a scheme block)\n```"
    );
    assert_eq!(
        chunks(prompt),
        vec![
            ("(turns 1)\n(pr #:draft #t)\n", 17),
            ("(on-stop \"Done?\")\n", 102),
        ]
    );
}

#[test]
fn an_unclosed_nori_fence_runs_to_the_end() {
    let prompt = "Go.\n```nori\n(turns 4)";
    assert_eq!(
        split(prompt),
        (
            "Go.".to_string(),
            vec![Chunk {
                text: "(turns 4)",
                offset: 12
            }]
        )
    );
}

/// An engine with `source` loaded, and its default workflow.
fn engine_with(source: &str) -> (ScriptEngine, Option<Workflow>) {
    let mut engine = ScriptEngine::new().expect("engine");
    engine.run(source).expect("loads");
    let registry = engine.registry();
    let default = registry
        .default
        .as_deref()
        .and_then(|name| registry.get(name))
        .cloned();
    (engine, default)
}

#[test]
fn clauses_apply_to_the_default_workflow_in_order() {
    let (mut engine, default) = engine_with(
        r#"
(define-workflow land (turns 3) (on-change "Commit it."))
(define-workflow strict (turns 1))
(define (careful w) (~> w (on-stop "Run the tests.")))
(default-workflow land)
"#,
    );
    let applied = apply(
        &mut engine,
        default.as_ref(),
        "Refactor the parser.\n,(turns 5)\n```nori\ncareful\n```\n",
    )
    .expect("applies");
    assert_eq!(
        applied,
        InlinePrompt {
            prompt: "Refactor the parser.".to_string(),
            workflow: Some(Workflow {
                max_turns: Some(5),
                hooks: vec![
                    Hook {
                        event: HookEvent::AfterChange,
                        prompt: "Commit it.".to_string(),
                    },
                    Hook {
                        event: HookEvent::BeforeStop,
                        prompt: "Run the tests.".to_string(),
                    },
                ],
                ..Workflow::named("land")
            }),
        }
    );
    let switched =
        apply(&mut engine, default.as_ref(), ",(use strict)\nQuick fix.").expect("applies");
    assert_eq!(
        switched.workflow,
        Some(Workflow {
            max_turns: Some(1),
            ..Workflow::named("land")
        })
    );
}

#[test]
fn without_a_default_clauses_build_a_prompt_workflow() {
    let (mut engine, _) = engine_with("");
    let applied = apply(
        &mut engine,
        None,
        ",(commit-agent 'claude #:model \"haiku\")\nCommit my work.",
    )
    .expect("applies");
    assert_eq!(
        applied,
        InlinePrompt {
            prompt: "Commit my work.".to_string(),
            workflow: Some(Workflow {
                commit: Some(CommitRules {
                    agent: Some(CommitAgent {
                        agent: "claude".to_string(),
                        model: Some("haiku".to_string()),
                    }),
                    ..CommitRules::default()
                }),
                ..Workflow::named(INLINE_WORKFLOW)
            }),
        }
    );
}

#[test]
fn inline_errors_point_into_the_prompt() {
    let (mut engine, _) = engine_with("");
    assert_eq!(
        apply(&mut engine, None, "Do it.\n,(turns \"many\")"),
        Err(ScriptError {
            origin: Origin::Prompt,
            kind: ErrorKind::Eval,
            line: Some(2),
            column: Some(3),
            message: "turns: the turn count must be a whole number of at least 0, got \"many\""
                .to_string(),
        })
    );
}

#[test]
fn inline_dsl_is_sandboxed_and_cannot_escape_the_wrapper() {
    let (mut engine, _) = engine_with("");
    let denied = apply(
        &mut engine,
        None,
        "x\n,(turns (string-length (env-var \"HOME\")))",
    )
    .expect_err("rejected");
    assert_eq!(
        (denied.kind, denied.line, denied.column),
        (ErrorKind::Sandbox, Some(2), Some(25))
    );
    let escape = apply(&mut engine, None, ",(turns 1)) (define x 1").expect_err("rejected");
    assert_eq!(
        (escape.kind, escape.line, escape.column),
        (ErrorKind::Syntax, Some(1), Some(11))
    );
}

#[test]
fn inline_dsl_cannot_define_or_register() {
    let (mut engine, _) = engine_with("(define-workflow land (turns 1))");
    let message =
        "inline DSL takes clauses such as (turns 3); put definitions in init.scm or .nori/nori.scm";
    for (prompt, column) in [
        (",(define x 1)", 3),
        (",(use (register-workflow (workflow \"sneaky\")))", 8),
    ] {
        assert_eq!(
            apply(&mut engine, None, prompt),
            Err(ScriptError {
                origin: Origin::Prompt,
                kind: ErrorKind::Sandbox,
                line: Some(1),
                column: Some(column),
                message: message.to_string(),
            }),
            "{prompt}"
        );
    }
    assert_eq!(
        engine.registry().workflows,
        vec![Workflow {
            max_turns: Some(1),
            ..Workflow::named("land")
        }]
    );
}

#[test]
fn inline_forms_must_be_clauses() {
    let (mut engine, _) = engine_with("");
    let error = apply(&mut engine, None, ",(+ 1 2)").expect_err("rejected");
    assert_eq!(error.kind, ErrorKind::Eval);
}

#[test]
fn loaded_applies_inline_dsl_in_a_fresh_engine() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("init.scm");
    std::fs::write(
        &path,
        "(define-workflow land (turns 3))\n(define-workflow quick (turns 1))\n(default-workflow land)",
    )
    .expect("write");
    let loaded = crate::load::load(Some(&path), None).expect("loads");
    // The scripts are kept as text: the file is not read again.
    drop(dir);
    assert_eq!(
        loaded.apply_inline("Hurry.\n,(use 'quick)\n,(on-stop \"Done?\")"),
        Ok(InlinePrompt {
            prompt: "Hurry.".to_string(),
            workflow: Some(Workflow {
                max_turns: Some(1),
                hooks: vec![Hook {
                    event: HookEvent::BeforeStop,
                    prompt: "Done?".to_string(),
                }],
                ..Workflow::named("land")
            }),
        })
    );
    assert_eq!(
        Loaded::default().apply_inline("No DSL."),
        Ok(InlinePrompt {
            prompt: "No DSL.".to_string(),
            workflow: None,
        })
    );
}
