//! Evaluate declarative conversation commands with the official Pkl runtime.
//!
//! Evaluation produces data only. It does not dispatch messages or execute
//! tools. Record commands and effect outcomes separately; never evaluate the
//! event log to replay a conversation.
pub mod machine;
mod policy;
pub use policy::global_policy_path;
pub use policy::load_policy;
pub use policy::load_policy_with_machine;
pub use policy::project_policy_path;

use std::collections::BTreeMap;
use std::path::Path;

use pklrust::EvaluatorManager;
use pklrust::EvaluatorOptions;
use pklrust::ModuleSource;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Operation {
    Prompt {
        text: String,
    },
    Send {
        recipient: String,
        message: String,
    },
    Spawn {
        actor: String,
        agent: String,
    },
    ToolRequest {
        id: String,
        tool: String,
        input: String,
    },
    ToolResult {
        id: String,
        output: String,
    },
    /// Explicit escape hatch for an external process; evaluation alone never runs it.
    Unsafe {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    Config {
        key: String,
        value: String,
    },
    Fork {
        actor: String,
        start: i64,
        end: i64,
    },
    Rewind {
        head: Option<i64>,
    },
    SelectContext {
        start: i64,
        end: i64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub id: i64,
    pub operation: Operation,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Pkl evaluation failed: {0}")]
    Evaluation(String),
    #[error("invalid conversation program: {0}")]
    Invalid(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Program {
    #[serde(default)]
    commands: Vec<serde_json::Value>,
    #[serde(default, rename = "unsafe")]
    unsafe_operation: Option<UnsafeDeclaration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnsafeDeclaration {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
}

/// Evaluate inline Pkl using the standard user configuration capabilities.
/// The Pkl executable must be available on PATH.
/// The starting ID is allocated by the log owner, making recompilation stable.
pub fn compile(source: &str, first_id: i64) -> Result<Vec<Command>, Error> {
    compile_with_pkl("pkl", source, first_id)
}

/// As [`compile`], using an explicit Pkl executable path.
pub fn compile_with_pkl(
    executable: &str,
    source: &str,
    first_id: i64,
) -> Result<Vec<Command>, Error> {
    compile_with_options(
        executable,
        source,
        first_id,
        EvaluatorOptions::preconfigured(),
    )
}

/// Evaluate with explicit Pkl capabilities, properties, and an optional timeout.
pub fn compile_with_options(
    executable: &str,
    source: &str,
    first_id: i64,
    options: EvaluatorOptions,
) -> Result<Vec<Command>, Error> {
    compile_module(executable, ModuleSource::text(source), first_id, options)
}

/// Evaluate an inline program using explicitly captured properties.
pub fn compile_with_properties(
    source: &str,
    first_id: i64,
    properties: BTreeMap<String, String>,
) -> Result<Vec<Command>, Error> {
    compile_with_options(
        "pkl",
        source,
        first_id,
        EvaluatorOptions::preconfigured().properties(properties.into_iter().collect()),
    )
}

/// Evaluate a Pkl file with its original URI, so relative imports resolve beside it.
pub fn compile_path(path: &Path, first_id: i64) -> Result<Vec<Command>, Error> {
    compile_path_with_properties(path, first_id, BTreeMap::new())
}

/// Evaluate a file with explicitly captured properties and no implicit timeout.
pub fn compile_path_with_properties(
    path: &Path,
    first_id: i64,
    properties: BTreeMap<String, String>,
) -> Result<Vec<Command>, Error> {
    compile_module(
        "pkl",
        ModuleSource::file(path),
        first_id,
        EvaluatorOptions::preconfigured().properties(properties.into_iter().collect()),
    )
}

/// Evaluate the recorded source with its original file URI for relative imports.
/// The main module is not read again after its intent has been journaled.
pub fn compile_source_at_path(
    source: &str,
    path: &Path,
    first_id: i64,
    properties: BTreeMap<String, String>,
) -> Result<Vec<Command>, Error> {
    let module = ModuleSource::text_with_uri(ModuleSource::file(path).module_uri(), source);
    compile_module(
        "pkl",
        module,
        first_id,
        EvaluatorOptions::preconfigured().properties(properties.into_iter().collect()),
    )
}

fn compile_module(
    executable: &str,
    source: ModuleSource,
    first_id: i64,
    options: EvaluatorOptions,
) -> Result<Vec<Command>, Error> {
    if first_id < 1 {
        return Err(Error::Invalid("first command ID must be positive".into()));
    }
    let mut manager =
        EvaluatorManager::with_command(executable).map_err(|e| Error::Evaluation(e.to_string()))?;
    let evaluator = manager
        .new_evaluator(options)
        .map_err(|e| Error::Evaluation(e.to_string()))?;
    let program: Program = manager
        .evaluate_module_typed(&evaluator, source)
        .map_err(|e| Error::Evaluation(e.to_string()))?;
    manager
        .close_evaluator(&evaluator)
        .map_err(|e| Error::Evaluation(e.to_string()))?;
    let mut operations = program
        .commands
        .into_iter()
        .map(|value| serde_json::from_value(value).map_err(|e| Error::Invalid(e.to_string())))
        .collect::<Result<Vec<Operation>, Error>>()?;
    if let Some(declaration) = program.unsafe_operation {
        operations.push(Operation::Unsafe {
            program: declaration.program,
            args: declaration.args,
            cwd: declaration.cwd,
        });
    }
    operations
        .into_iter()
        .enumerate()
        .map(|(index, operation)| {
            validate(&operation)?;
            let id = i64::try_from(index)
                .ok()
                .and_then(|index| first_id.checked_add(index))
                .ok_or_else(|| Error::Invalid("command ID overflow".into()))?;
            Ok(Command { id, operation })
        })
        .collect()
}

fn validate(op: &Operation) -> Result<(), Error> {
    let nonempty = |items: &[&str]| {
        if items.iter().any(|s| s.trim().is_empty()) {
            Err(Error::Invalid("operation strings must not be empty".into()))
        } else {
            Ok(())
        }
    };
    let range = |start: i64, end: i64| {
        if start < 1 || start > end {
            Err(Error::Invalid("invalid inclusive event range".into()))
        } else {
            Ok(())
        }
    };
    match op {
        Operation::Prompt { text } => nonempty(&[text]),
        Operation::Send { recipient, message } => nonempty(&[recipient, message]),
        Operation::Spawn { actor, agent } => nonempty(&[actor, agent]),
        Operation::ToolRequest { id, tool, input } => nonempty(&[id, tool, input]),
        Operation::ToolResult { id, output } => nonempty(&[id, output]),
        Operation::Unsafe { program, .. } => nonempty(&[program]),
        Operation::Config { key, value } => nonempty(&[key, value]),
        Operation::Fork { actor, start, end } => {
            nonempty(&[actor])?;
            range(*start, *end)
        }
        Operation::Rewind { head } => {
            if head.is_some_and(|head| head < 1) {
                Err(Error::Invalid("invalid head".into()))
            } else {
                Ok(())
            }
        }
        Operation::SelectContext { start, end } => range(*start, *end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn compiles_pkl_to_typed_ordered_commands() {
        let source = include_str!("../examples/conversation.pkl");
        assert_eq!(
            compile(source, 1).unwrap(),
            vec![
                Command {
                    id: 1,
                    operation: Operation::Prompt {
                        text: "Investigate the failure".into()
                    }
                },
                Command {
                    id: 2,
                    operation: Operation::Spawn {
                        actor: "reviewer".into(),
                        agent: "elizacp".into()
                    }
                },
                Command {
                    id: 3,
                    operation: Operation::SelectContext { start: 1, end: 1 }
                },
                Command {
                    id: 4,
                    operation: Operation::Fork {
                        actor: "self".into(),
                        start: 1,
                        end: 1
                    }
                },
                Command {
                    id: 5,
                    operation: Operation::Send {
                        recipient: "reviewer".into(),
                        message: "Review this context".into()
                    }
                },
                Command {
                    id: 6,
                    operation: Operation::Unsafe {
                        program: "printf".into(),
                        args: vec!["Explicit external diagnostic\n".into()],
                        cwd: None,
                    }
                },
            ]
        );
    }

    #[test]
    fn rejects_invalid_command_and_forbids_resource_reads() {
        let restricted = |source| {
            compile_with_options(
                "pkl",
                source,
                1,
                EvaluatorOptions::new()
                    .allowed_modules(vec!["^pkl:".into(), "^repl:".into()])
                    .allowed_resources(Vec::new()),
            )
        };
        assert!(
            compile(
                "commands = List(new Dynamic { op = \"fork\"; actor = \"a\"; start = 2; end = 1 })",
                1
            )
            .is_err()
        );
        assert!(restricted("commands = List(new Dynamic { op = \"prompt\"; text = read(\"file:///etc/passwd\") })").is_err());
        assert!(
            restricted(
                "commands = List(new Dynamic { op = \"prompt\"; text = read(\"env:HOME\") })"
            )
            .is_err()
        );
        assert!(
            restricted("import \"file:///etc/passwd\" as stolen\ncommands = stolen.commands")
                .is_err()
        );
    }
    #[test]
    fn compiling_unsafe_is_data_only_and_defaults_optional_fields() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-exist");
        let source = format!(
            "unsafe {{ program = \"touch\"; args = List(\"{}\") }}",
            marker.display()
        );
        assert_eq!(
            compile(&source, 7).unwrap(),
            vec![Command {
                id: 7,
                operation: Operation::Unsafe {
                    program: "touch".into(),
                    args: vec![marker.display().to_string()],
                    cwd: None
                }
            }]
        );
        assert!(!marker.exists());
        assert_eq!(
            compile("unsafe { program = \"git\" }", 1).unwrap(),
            vec![Command {
                id: 1,
                operation: Operation::Unsafe {
                    program: "git".into(),
                    args: Vec::new(),
                    cwd: None
                }
            }]
        );
    }

    #[test]
    fn file_compilation_resolves_relative_imports_and_captured_properties() {
        let dir = tempfile::tempdir().unwrap();
        let dependency = dir.path().join("dependency.pkl");
        let program = dir.path().join("program.pkl");
        std::fs::write(dependency, "message = \"from import\"").unwrap();
        std::fs::write(&program, r#"import "dependency.pkl" as dependency
commands = List(new Dynamic { op = "prompt"; text = dependency.message + read("prop:machine.cpu.available_parallelism") })"#).unwrap();
        let properties = BTreeMap::from([("machine.cpu.available_parallelism".into(), "7".into())]);
        assert_eq!(
            compile_path_with_properties(&program, 3, properties).unwrap(),
            vec![Command {
                id: 3,
                operation: Operation::Prompt {
                    text: "from import7".into()
                }
            }]
        );
    }
    #[test]
    fn recorded_source_keeps_file_base_without_rereading_main_module() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("program.pkl");
        std::fs::write(dir.path().join("dependency.pkl"), "message = \"imported\"").unwrap();
        std::fs::write(
            &main,
            "commands = List(new Dynamic { op = \"prompt\"; text = \"changed on disk\" })",
        )
        .unwrap();
        let recorded = r#"import "dependency.pkl" as dependency
commands = List(new Dynamic { op = "prompt"; text = "recorded " + dependency.message + read("prop:machine.cpu.available_parallelism") })"#;
        let properties = BTreeMap::from([("machine.cpu.available_parallelism".into(), "7".into())]);
        assert_eq!(
            compile_source_at_path(recorded, &main, 42, properties).unwrap(),
            vec![Command {
                id: 42,
                operation: Operation::Prompt {
                    text: "recorded imported7".into()
                }
            }],
        );
    }
}
