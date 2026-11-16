//! A long-lived thread that owns one script engine.
//!
//! Creating a Steel engine is the expensive part of loading, and engines are
//! not `Send`. The host creates one on its own thread as soon as it is
//! spawned, so the cost stays off the caller's path, and reuses it for every
//! inline prompt.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use crate::engine::ScriptEngine;
use crate::error::ErrorKind;
use crate::error::Origin;
use crate::error::ScriptError;
use crate::inline;
use crate::inline::InlinePrompt;
use crate::load::Loaded;
use crate::load::load_scripts;

enum Request {
    Loaded(mpsc::Sender<Result<Loaded, ScriptError>>),
    Inline(String, mpsc::Sender<Result<InlinePrompt, ScriptError>>),
}

/// A handle to the script thread. Cheap to clone; the thread exits when the
/// last handle is dropped.
#[derive(Clone)]
pub struct ScriptHost {
    requests: mpsc::Sender<Request>,
}

impl ScriptHost {
    /// Starts the script thread, which loads `global` and then `project` at
    /// once (see [`load`](crate::load)). Pass a project path only for a
    /// trusted directory.
    pub fn spawn(global: Option<PathBuf>, project: Option<PathBuf>) -> std::io::Result<Self> {
        let (requests, inbox) = mpsc::channel();
        thread::Builder::new()
            .name("nori-script".to_string())
            .spawn(move || serve(global, project, &inbox))?;
        Ok(Self { requests })
    }

    /// The loaded workflows, waiting for loading to finish if it has not.
    pub fn loaded(&self) -> Result<Loaded, ScriptError> {
        self.ask(Request::Loaded)
    }

    /// Removes workflow DSL from `prompt` and applies it to the default
    /// workflow. Returns at once, without the script thread, when the prompt
    /// has no DSL.
    pub fn apply_inline(&self, prompt: &str) -> Result<InlinePrompt, ScriptError> {
        if !inline::has_inline_dsl(prompt) {
            return Ok(InlinePrompt {
                prompt: prompt.to_string(),
                workflow: None,
            });
        }
        let prompt = prompt.to_string();
        self.ask(|reply| Request::Inline(prompt, reply))
    }

    fn ask<T>(
        &self,
        request: impl FnOnce(mpsc::Sender<Result<T, ScriptError>>) -> Request,
    ) -> Result<T, ScriptError> {
        let stopped = || {
            ScriptError::new(
                Origin::Prompt,
                ErrorKind::Eval,
                "the script thread has stopped",
            )
        };
        let (reply, answer) = mpsc::channel();
        self.requests.send(request(reply)).map_err(|_| stopped())?;
        answer.recv().map_err(|_| stopped())?
    }
}

fn serve(global: Option<PathBuf>, project: Option<PathBuf>, inbox: &mpsc::Receiver<Request>) {
    let (mut engine, loaded) = match load_scripts(global.as_deref(), project.as_deref()) {
        Ok((engine, loaded)) => (engine, Ok(loaded)),
        // Inline DSL still works after a failed load, in a clean engine.
        Err(error) => (None, Err(error)),
    };
    for request in inbox {
        match request {
            Request::Loaded(reply) => {
                let _ = reply.send(loaded.clone());
            }
            Request::Inline(prompt, reply) => {
                let engine = match engine.take().map_or_else(ScriptEngine::new, Ok) {
                    Ok(created) => engine.insert(created),
                    Err(failure) => {
                        let _ = reply.send(Err(failure.locate(Origin::Prompt, "")));
                        continue;
                    }
                };
                let default = loaded.as_ref().ok().and_then(Loaded::default_workflow);
                let _ = reply.send(inline::apply(engine, default, &prompt));
            }
        }
    }
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
