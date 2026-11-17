//! Loading `$NORI_HOME/init.scm` and `<repo>/.nori/nori.scm`.

use std::io;
use std::path::Path;
use std::path::PathBuf;

use crate::engine::ScriptEngine;
use crate::error::ErrorKind;
use crate::error::Origin;
use crate::error::ScriptError;
use crate::model::Workflow;

pub(crate) const DEFAULT_SCRIPT: &str = include_str!("../default.scm");

/// The global script's name inside `$NORI_HOME`.
pub const GLOBAL_SCRIPT: &str = "init.scm";

/// The project script's path inside a repository root.
pub const PROJECT_SCRIPT: &str = ".nori/nori.scm";

/// `$NORI_HOME/init.scm`.
pub fn global_script_path(nori_home: &Path) -> PathBuf {
    nori_home.join(GLOBAL_SCRIPT)
}

/// `<repo>/.nori/nori.scm`. Load it only for a trusted directory.
pub fn project_script_path(repo_root: &Path) -> PathBuf {
    repo_root.join(PROJECT_SCRIPT)
}

/// A script file's text, kept so inline DSL can be evaluated against the same
/// definitions later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Script {
    pub path: PathBuf,
    pub text: String,
}

impl Script {
    pub(crate) fn run(&self, engine: &mut ScriptEngine) -> Result<(), ScriptError> {
        engine
            .run(&self.text)
            .map(drop)
            .map_err(|failure| failure.locate(Origin::File(self.path.clone()), &self.text))
    }
}

/// The workflows defined by the loaded scripts.
///
/// Plain data, with no engine inside: `Send`, `Sync`, and cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Loaded {
    workflows: Vec<Workflow>,
    default: Option<String>,
    pub(crate) scripts: Vec<Script>,
    pub(crate) builtin_defaults: bool,
}

impl Loaded {
    /// Every registered workflow, in the order first registered.
    pub fn workflows(&self) -> &[Workflow] {
        &self.workflows
    }

    pub fn workflow(&self, name: &str) -> Option<&Workflow> {
        self.workflows.iter().find(|workflow| workflow.name == name)
    }

    /// The workflow named by `default-workflow`, if any script set one.
    pub fn default_workflow(&self) -> Option<&Workflow> {
        self.default.as_deref().and_then(|name| self.workflow(name))
    }

    /// The script files that were found and loaded, global first.
    pub fn script_paths(&self) -> impl Iterator<Item = &Path> {
        self.scripts.iter().map(|script| script.path.as_path())
    }
}

/// Loads the global script, then the project script, into one engine.
///
/// The project script runs second, so it sees the global definitions and
/// overrides them: re-registering a workflow name replaces it, and its
/// `default-workflow` wins. A path that does not exist is skipped; with no
/// script at all, no engine is created. Pass a project path only for a
/// trusted directory.
///
/// This blocks for as long as engine creation takes; see
/// [`ScriptHost`](crate::ScriptHost) to load off the caller's thread.
pub fn load(global: Option<&Path>, project: Option<&Path>) -> Result<Loaded, ScriptError> {
    load_scripts(global, project).map(|(_, loaded)| loaded)
}

/// [`load`], also returning the engine the scripts ran in (`None` when there
/// were no scripts).
pub(crate) fn load_scripts(
    global: Option<&Path>,
    project: Option<&Path>,
) -> Result<(Option<ScriptEngine>, Loaded), ScriptError> {
    load_scripts_with_defaults(global, project, false)
}

pub(crate) fn load_scripts_with_defaults(
    global: Option<&Path>,
    project: Option<&Path>,
    builtin_defaults: bool,
) -> Result<(Option<ScriptEngine>, Loaded), ScriptError> {
    let mut scripts = Vec::new();
    for path in [global, project].into_iter().flatten() {
        match std::fs::read_to_string(path) {
            Ok(text) => scripts.push(Script {
                path: path.to_path_buf(),
                text,
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ScriptError::new(
                    Origin::File(path.to_path_buf()),
                    ErrorKind::Io,
                    format!("could not read the script: {error}"),
                ));
            }
        }
    }
    if scripts.is_empty() && !builtin_defaults {
        return Ok((None, Loaded::default()));
    }
    let mut engine = ScriptEngine::new().map_err(|failure| {
        failure.locate(
            scripts
                .first()
                .map_or(Origin::Prompt, |first| Origin::File(first.path.clone())),
            "",
        )
    })?;
    if builtin_defaults {
        engine
            .run(DEFAULT_SCRIPT)
            .map_err(|failure| failure.locate(Origin::Prompt, DEFAULT_SCRIPT))?;
    }
    for script in &scripts {
        script.run(&mut engine)?;
    }
    let registry = engine.registry();
    let loaded = Loaded {
        workflows: registry.workflows,
        default: registry.default,
        scripts,
        builtin_defaults,
    };
    Ok((Some(engine), loaded))
}

#[cfg(test)]
#[path = "load_tests.rs"]
mod tests;
