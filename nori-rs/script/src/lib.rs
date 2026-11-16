//! Nori's workflow DSL: a small Scheme vocabulary, run on an embedded and
//! sandboxed [Steel](https://github.com/mattwparas/steel) engine, that says how
//! an agent session behaves: how many turns it may run on its own, what Nori
//! prompts it with at session events, and its commit and pull-request rules.
//!
//! The setup follows Helix's Steel plugin system: a global script
//! (`$NORI_HOME/init.scm`) and a project script (`<repo>/.nori/nori.scm`) run
//! in one engine with a builtin module (`nori/workflow`) already loaded.
//! The output is plain data ([`Workflow`]); nothing downstream needs Scheme.
//!
//! ```no_run
//! # use std::path::Path;
//! // At startup: loading happens on the host's own thread.
//! let host = nori_script::ScriptHost::spawn(
//!     Some(nori_script::global_script_path(Path::new("/home/me/.nori"))),
//!     Some(nori_script::project_script_path(Path::new("/src/app"))),
//! )?;
//! // Per prompt: strip inline DSL and get the workflow for this turn.
//! let turn = host.apply_inline("Fix the flaky test.\n,(turns 2)")?;
//! let workflow = turn.workflow.or(host.loaded()?.default_workflow().cloned());
//! if let Some(workflow) = workflow {
//!     println!("{}", workflow.instructions());
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! See `docs/workflows.md` for the vocabulary and the sandbox rules.

mod dsl;
mod engine;
mod error;
mod host;
mod inline;
mod load;
mod model;
mod render;
mod sandbox;

pub use engine::TIME_BUDGET;
pub use error::ErrorKind;
pub use error::Origin;
pub use error::ScriptError;
pub use host::ScriptHost;
pub use inline::INLINE_WORKFLOW;
pub use inline::InlinePrompt;
pub use inline::has_inline_dsl;
pub use load::GLOBAL_SCRIPT;
pub use load::Loaded;
pub use load::PROJECT_SCRIPT;
pub use load::global_script_path;
pub use load::load;
pub use load::project_script_path;
pub use model::CommitAgent;
pub use model::CommitBatch;
pub use model::CommitRules;
pub use model::CommitStyle;
pub use model::Hook;
pub use model::HookEvent;
pub use model::PrRules;
pub use model::Upstream;
pub use model::Workflow;
