//! Workflow DSL embedded in a user's prompt.
//!
//! Two spellings, neither of which occurs in ordinary prose or code:
//!
//! - a fenced block whose info string is exactly `nori`;
//! - a line that starts, at column 0 and outside any other fenced block, with
//!   `,(`.
//!
//! Their forms are clauses applied in order to the session's workflow, as if
//! written inside `(~> workflow ...)`.

use crate::dsl;
use crate::engine::ScriptEngine;
use crate::error::ErrorKind;
use crate::error::Failure;
use crate::error::Origin;
use crate::error::ScriptError;
use crate::load::Loaded;
use crate::model::Workflow;

/// The name inline clauses are threaded onto.
const BASE: &str = "*nori-inline-base*";

/// The workflow name used for inline clauses when no default is defined.
pub const INLINE_WORKFLOW: &str = "prompt";

/// A prompt with its DSL removed, and what the DSL produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlinePrompt {
    /// The prompt to send to the agent.
    pub prompt: String,
    /// The default workflow (or an empty one named `prompt`) with the inline
    /// clauses applied. `None` when the prompt had no DSL.
    pub workflow: Option<Workflow>,
}

/// DSL text found in a prompt, and the byte offset where it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Chunk<'a> {
    text: &'a str,
    offset: usize,
}

/// Whether `prompt` contains workflow DSL. Cheap; creates no engine.
pub fn has_inline_dsl(prompt: &str) -> bool {
    !split(prompt).1.is_empty()
}

impl Loaded {
    /// Removes workflow DSL from `prompt` and applies its clauses to the
    /// default workflow (or to an empty one named [`INLINE_WORKFLOW`]).
    ///
    /// A prompt without DSL returns at once. Otherwise this creates a fresh
    /// engine and re-runs the scripts on the calling thread, which costs as
    /// much as [`load`](crate::load): call it from a blocking context, or use
    /// [`ScriptHost::apply_inline`](crate::ScriptHost::apply_inline), which
    /// reuses one warm engine.
    pub fn apply_inline(&self, prompt: &str) -> Result<InlinePrompt, ScriptError> {
        if !has_inline_dsl(prompt) {
            return Ok(InlinePrompt {
                prompt: prompt.to_string(),
                workflow: None,
            });
        }
        let mut engine =
            ScriptEngine::new().map_err(|failure| failure.locate(Origin::Prompt, ""))?;
        for script in &self.scripts {
            script.run(&mut engine)?;
        }
        apply(&mut engine, self.default_workflow(), prompt)
    }
}

/// Removes workflow DSL from `prompt` and applies its clauses, in `engine`, to
/// `default` (or to an empty workflow named [`INLINE_WORKFLOW`]). Leaves the
/// engine's registry as it found it.
pub(crate) fn apply(
    engine: &mut ScriptEngine,
    default: Option<&Workflow>,
    prompt: &str,
) -> Result<InlinePrompt, ScriptError> {
    let (kept, chunks) = split(prompt);
    if chunks.is_empty() {
        return Ok(InlinePrompt {
            prompt: prompt.to_string(),
            workflow: None,
        });
    }
    let registry = engine.registry();
    let applied = apply_chunks(engine, default, prompt, chunks);
    engine.restore(registry);
    Ok(InlinePrompt {
        prompt: kept,
        workflow: Some(applied?),
    })
}

fn apply_chunks(
    engine: &mut ScriptEngine,
    default: Option<&Workflow>,
    prompt: &str,
    chunks: Vec<Chunk<'_>>,
) -> Result<Workflow, ScriptError> {
    let mut workflow = default
        .cloned()
        .unwrap_or_else(|| Workflow::named(INLINE_WORKFLOW));
    let prefix = format!("(~> {BASE}\n");
    for chunk in chunks {
        // Maps a failure `shift` bytes into the evaluated text back to the
        // prompt.
        let locate = |failure: Failure, shift: usize| {
            let offset = failure
                .offset
                .map(|offset| chunk.offset + offset.saturating_sub(shift).min(chunk.text.len()));
            Failure::new(failure.kind, offset, failure.message).locate(Origin::Prompt, prompt)
        };
        // Check the chunk alone first, so an unbalanced `)` cannot close the
        // wrapper and escape it.
        crate::sandbox::check_clauses(chunk.text).map_err(|failure| locate(failure, 0))?;
        let base = dsl::value_of(workflow).map_err(|error| {
            ScriptError::new(Origin::Prompt, ErrorKind::Eval, error.to_string())
        })?;
        engine.bind(BASE, base);
        let source = format!("{prefix}{}\n)", chunk.text);
        let values = engine
            .run(&source)
            .map_err(|failure| locate(failure, prefix.len()))?;
        workflow = values.last().and_then(dsl::workflow_of).ok_or_else(|| {
            ScriptError::new(
                Origin::Prompt,
                ErrorKind::Eval,
                "inline forms must be workflow clauses such as (turns 3)",
            )
            .at(prompt, chunk.offset)
        })?;
    }
    Ok(workflow)
}

/// The prompt without its DSL lines, and the DSL chunks in order.
fn split(prompt: &str) -> (String, Vec<Chunk<'_>>) {
    enum State {
        Prose,
        /// Inside some other fenced block, opened by this fence.
        Code {
            fence: char,
            length: usize,
        },
        /// Inside a ```nori block that started at this byte offset.
        Nori {
            start: usize,
            fence: char,
            length: usize,
        },
    }
    let mut kept = String::with_capacity(prompt.len());
    let mut chunks = Vec::new();
    let mut state = State::Prose;
    let mut offset = 0;
    for line in prompt.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let content = line.trim_end_matches(['\n', '\r']);
        state = match state {
            State::Prose => match fence(content) {
                Some((fence, length, "nori")) => State::Nori {
                    start: offset,
                    fence,
                    length,
                },
                Some((fence, length, _)) => {
                    kept.push_str(line);
                    State::Code { fence, length }
                }
                None => {
                    if let Some(form) = content.strip_prefix(',') {
                        if form.starts_with('(') {
                            chunks.push(Chunk {
                                text: form,
                                offset: line_start + 1,
                            });
                        } else {
                            kept.push_str(line);
                        }
                    } else {
                        kept.push_str(line);
                    }
                    State::Prose
                }
            },
            State::Code {
                fence: open,
                length,
            } => {
                kept.push_str(line);
                if closes(content, open, length) {
                    State::Prose
                } else {
                    State::Code {
                        fence: open,
                        length,
                    }
                }
            }
            State::Nori {
                start,
                fence: open,
                length,
            } => {
                if closes(content, open, length) {
                    chunks.push(Chunk {
                        text: &prompt[start..line_start],
                        offset: start,
                    });
                    State::Prose
                } else {
                    State::Nori {
                        start,
                        fence: open,
                        length,
                    }
                }
            }
        };
    }
    // An unclosed ```nori block runs to the end of the prompt.
    if let State::Nori { start, .. } = state {
        chunks.push(Chunk {
            text: &prompt[start..],
            offset: start,
        });
    }
    if chunks.is_empty() {
        return (prompt.to_string(), chunks);
    }
    (kept.trim().to_string(), chunks)
}

/// The fence character, its length, and the info string, when `line` opens a
/// fenced code block (up to three spaces of indentation, per CommonMark).
fn fence(line: &str) -> Option<(char, usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let fence = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let length = rest.len() - rest.trim_start_matches(fence).len();
    if length < 3 {
        return None;
    }
    Some((fence, length, rest[length..].trim()))
}

fn closes(line: &str, open: char, length: usize) -> bool {
    matches!(fence(line), Some((fence, closing, "")) if fence == open && closing >= length)
}

#[cfg(test)]
#[path = "inline_tests.rs"]
mod tests;
