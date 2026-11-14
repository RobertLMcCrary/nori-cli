//! Undo support for the ACP backend through an edit journal.
//!
//! ACP hands the harness every file edit the agent reports as a diff. The
//! [`EditJournal`] records, per turn, what each reported edit replaced, and
//! [`handle_undo`] / [`handle_undo_to`] write that content back. Nothing here
//! touches git and nothing runs on the prompt-submit path: turns are opened and
//! edits are recorded on the session runtime task, and restoring runs only when
//! the user asks for `/undo`.
//!
//! This is a deliberate reduction from tree snapshots. Edits the agent makes
//! without reporting a diff (for example through a shell command) are not
//! reversible, and an undo says so when the turn ran commands.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::UndoEvent;
use tokio::sync::mpsc;
use tracing::info;
use tracing::warn;

use crate::backend::BackendEvent;
use crate::normalized::Artifact;
use crate::normalized::FileOperation;
use crate::normalized::Invocation;
use crate::normalized::ToolKind;
use crate::normalized::ToolSnapshot;

/// Files larger than this are not read to capture the content an overwrite
/// replaced.
const MAX_CAPTURED_FILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoSnapshot {
    pub index: i64,
    /// Short tag shown next to the label, e.g. `2 files`.
    pub short_id: String,
    pub label: String,
}

/// What a file held before the agent's edit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    /// The file held this content (`None`: it did not exist).
    Known(Option<String>),
    /// The agent reported no usable previous content, so the edit cannot be
    /// reversed safely.
    Unknown,
    /// The previous content is being read from disk; until that finishes the
    /// edit counts as unknown.
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileEdit {
    path: PathBuf,
    before: Origin,
    /// Content after the edit (`None`: the file was deleted). For diffs that
    /// carry only the changed region this is that region.
    after: Option<String>,
}

#[derive(Debug, Default)]
struct CallEdits {
    edits: Vec<FileEdit>,
    /// Position in `edits` of each path, so a later snapshot of the same call finds its edit
    /// without scanning.
    edit_index: HashMap<PathBuf, usize>,
}

#[derive(Debug)]
struct TurnEdits {
    /// Identifies the turn across the background reads that finish after it was recorded.
    id: u64,
    label: String,
    calls: Vec<CallEdits>,
    /// Position in `calls` of each tool call, so a later snapshot of the same call finds it
    /// without scanning.
    call_index: HashMap<String, usize>,
    ran_commands: bool,
}

/// A file read that completes an edit's [`Origin::Pending`].
struct Capture {
    turn_id: u64,
    call: usize,
    edit: usize,
    path: PathBuf,
    after: Option<String>,
}

impl TurnEdits {
    fn touched_paths(&self) -> usize {
        let mut paths: Vec<&Path> = self
            .calls
            .iter()
            .flat_map(|call| call.edits.iter().map(|edit| edit.path.as_path()))
            .collect();
        paths.sort_unstable();
        paths.dedup();
        paths.len()
    }

    fn is_undoable(&self) -> bool {
        !self.calls.is_empty() || self.ran_commands
    }
}

/// Error returned by [`EditJournal::take_through_index`] when the requested
/// index cannot be fulfilled.
pub enum RestoreError {
    /// The journal holds no undoable turn.
    Empty,
    /// The index is out of bounds for the number of undoable turns.
    OutOfBounds,
}

/// Outcome of writing one turn's recorded content back to disk.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: Vec<PathBuf>,
    pub skipped: Vec<(PathBuf, String)>,
    pub ran_commands: bool,
}

/// Per-turn record of the file edits the agent reported.
#[derive(Default)]
pub struct EditJournal {
    turns: Mutex<Vec<TurnEdits>>,
    next_turn_id: std::sync::atomic::AtomicU64,
}

impl EditJournal {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<TurnEdits>> {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Open a turn. Edits reported until the next call belong to it.
    pub fn begin_turn(&self, label: String) {
        let id = self
            .next_turn_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.lock().push(TurnEdits {
            id,
            label,
            calls: Vec::new(),
            call_index: HashMap::new(),
            ran_commands: false,
        });
    }

    /// Record the file edits carried by a tool snapshot in the open turn.
    ///
    /// `scope` names the subagent session the snapshot came from, so that call ids of different
    /// sessions cannot collide; a child's edits belong to the current root turn.
    ///
    /// The cost is proportional to the size of the snapshot's diffs. Nothing here waits on the
    /// disk: when the agent reported no previous content, the file is read by a background task
    /// and the edit counts as unknown until that finishes. Snapshots arriving with no open turn
    /// (session replay) are ignored.
    pub fn record(self: &Arc<Self>, snapshot: &ToolSnapshot, cwd: &Path, scope: Option<&str>) {
        let reported = reported_edits(snapshot, cwd);
        let mut captures = Vec::new();
        {
            let mut turns = self.lock();
            let Some(turn) = turns.last_mut() else {
                return;
            };
            if snapshot.kind == ToolKind::Execute {
                turn.ran_commands = true;
            }
            if reported.is_empty() {
                return;
            }
            let key = match scope {
                Some(scope) => format!("{scope}/{}", snapshot.call_id),
                None => snapshot.call_id.clone(),
            };
            let call_position = match turn.call_index.get(&key) {
                Some(position) => *position,
                None => {
                    turn.calls.push(CallEdits::default());
                    turn.call_index.insert(key, turn.calls.len() - 1);
                    turn.calls.len() - 1
                }
            };
            let turn_id = turn.id;
            let call = &mut turn.calls[call_position];
            for reported in reported {
                // A later snapshot of the same call refines the result; what the file held
                // before the call does not change.
                if let Some(position) = call.edit_index.get(&reported.path) {
                    call.edits[*position].after = reported.after;
                    continue;
                }
                let position = call.edits.len();
                call.edit_index.insert(reported.path.clone(), position);
                let before = match reported.reported_before {
                    Some(before) => Origin::Known(before),
                    None => {
                        captures.push(Capture {
                            turn_id,
                            call: call_position,
                            edit: position,
                            path: reported.path.clone(),
                            after: reported.after.clone(),
                        });
                        Origin::Pending
                    }
                };
                call.edits.push(FileEdit {
                    path: reported.path,
                    before,
                    after: reported.after,
                });
            }
        }
        for capture in captures {
            let journal = Arc::clone(self);
            let read = move || {
                let origin = capture_origin(&capture.path, capture.after.as_deref());
                journal.complete_capture(&capture, origin);
            };
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    drop(runtime.spawn_blocking(read));
                }
                Err(_) => read(),
            }
        }
    }

    fn complete_capture(&self, capture: &Capture, origin: Origin) {
        let mut turns = self.lock();
        if let Some(edit) = turns
            .iter_mut()
            .rev()
            .find(|turn| turn.id == capture.turn_id)
            .and_then(|turn| turn.calls.get_mut(capture.call))
            .and_then(|call| call.edits.get_mut(capture.edit))
            .filter(|edit| edit.before == Origin::Pending)
        {
            edit.before = origin;
        }
    }

    /// Undoable turns, most recent first.
    pub fn list(&self) -> Vec<UndoSnapshot> {
        self.lock()
            .iter()
            .rev()
            .filter(|turn| turn.is_undoable())
            .enumerate()
            .map(|(index, turn)| {
                let files = turn.touched_paths();
                let short_id = match (files, turn.ran_commands) {
                    (0, _) => "commands only".to_string(),
                    (1, _) => "1 file".to_string(),
                    (files, _) => format!("{files} files"),
                };
                UndoSnapshot {
                    index: index as i64,
                    short_id,
                    label: turn.label.clone(),
                }
            })
            .collect()
    }

    /// Remove the undoable turn at the display index (0 = most recent) and all
    /// newer turns, returning them newest first.
    fn take_through_index(&self, index: i64) -> Result<Vec<TurnEdits>, RestoreError> {
        let mut turns = self.lock();
        let undoable: Vec<usize> = turns
            .iter()
            .enumerate()
            .filter(|(_, turn)| turn.is_undoable())
            .map(|(position, _)| position)
            .collect();
        if undoable.is_empty() {
            return Err(RestoreError::Empty);
        }
        let Some(display) = usize::try_from(index)
            .ok()
            .filter(|display| *display < undoable.len())
        else {
            return Err(RestoreError::OutOfBounds);
        };
        let cut = undoable[undoable.len() - 1 - display];
        let mut removed = turns.split_off(cut);
        removed.reverse();
        Ok(removed)
    }
}

struct ReportedEdit {
    path: PathBuf,
    /// `Some` when the agent reported the previous content explicitly.
    reported_before: Option<Option<String>>,
    after: Option<String>,
}

/// What a file holds before an edit whose previous content the agent did not report.
///
/// The file's current state is only trustworthy if it differs from the edit's result, because
/// only then has the edit not landed yet.
fn capture_origin(path: &Path, after: Option<&str>) -> Origin {
    match std::fs::metadata(path) {
        Err(_) => Origin::Known(None),
        Ok(metadata) if metadata.len() > MAX_CAPTURED_FILE_BYTES => Origin::Unknown,
        Ok(_) => match std::fs::read_to_string(path) {
            Ok(current) if after != Some(current.as_str()) => Origin::Known(Some(current)),
            Ok(_) | Err(_) => Origin::Unknown,
        },
    }
}

fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn reported_edits(snapshot: &ToolSnapshot, cwd: &Path) -> Vec<ReportedEdit> {
    let mut edits: Vec<ReportedEdit> = snapshot
        .artifacts
        .iter()
        .filter_map(|artifact| match artifact {
            Artifact::Diff(change) => Some(ReportedEdit {
                path: absolute(&change.path, cwd),
                reported_before: change.old_text.clone().map(Some),
                after: Some(change.new_text.clone()),
            }),
            Artifact::Text { .. } => None,
        })
        .collect();
    if !edits.is_empty() {
        return edits;
    }
    let Some(Invocation::FileOperations { operations }) = &snapshot.invocation else {
        return edits;
    };
    for operation in operations {
        match operation {
            FileOperation::Create { path, new_text } => edits.push(ReportedEdit {
                path: absolute(path, cwd),
                reported_before: None,
                after: Some(new_text.clone()),
            }),
            FileOperation::Update {
                path,
                old_text,
                new_text,
            } => edits.push(ReportedEdit {
                path: absolute(path, cwd),
                reported_before: Some(Some(old_text.clone())),
                after: Some(new_text.clone()),
            }),
            FileOperation::Delete { path, old_text } => {
                // Without the deleted content the file cannot be brought back.
                if old_text.is_some() {
                    edits.push(ReportedEdit {
                        path: absolute(path, cwd),
                        reported_before: Some(old_text.clone()),
                        after: None,
                    });
                }
            }
            FileOperation::Move {
                from_path,
                to_path,
                old_text,
                new_text,
            } => {
                if old_text.is_some() {
                    edits.push(ReportedEdit {
                        path: absolute(from_path, cwd),
                        reported_before: Some(old_text.clone()),
                        after: None,
                    });
                }
                if let Some(new_text) = new_text.as_ref().or(old_text.as_ref()) {
                    edits.push(ReportedEdit {
                        path: absolute(to_path, cwd),
                        reported_before: None,
                        after: Some(new_text.clone()),
                    });
                }
            }
        }
    }
    edits
}

/// Write one turn's recorded content back to disk.
///
/// Edits to a path are unwound newest first. An edit is reversed when the file
/// still holds exactly what the edit produced, or, for a diff that carries only
/// the changed region, still contains that region. Anything else was changed
/// since and is left alone.
fn restore_turn(turn: &TurnEdits) -> RestoreReport {
    let mut report = RestoreReport {
        ran_commands: turn.ran_commands,
        ..RestoreReport::default()
    };
    let mut by_path: Vec<(&Path, Vec<&FileEdit>)> = Vec::new();
    let mut positions: HashMap<&Path, usize> = HashMap::new();
    for edit in turn.calls.iter().flat_map(|call| call.edits.iter()) {
        let position = *positions.entry(edit.path.as_path()).or_insert_with(|| {
            by_path.push((edit.path.as_path(), Vec::new()));
            by_path.len() - 1
        });
        by_path[position].1.push(edit);
    }
    for (path, edits) in by_path {
        let mut state: Option<String> = match std::fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                report.skipped.push((path.to_path_buf(), error.to_string()));
                continue;
            }
        };
        let mut failure = None;
        for edit in edits.iter().rev() {
            let Origin::Known(before) = &edit.before else {
                failure = Some("the original content was not reported".to_string());
                break;
            };
            if state == edit.after {
                state = before.clone();
            } else if let (Some(current), Some(after), Some(before)) =
                (state.as_deref(), edit.after.as_deref(), before.as_deref())
                && !after.is_empty()
                && current.contains(after)
            {
                state = Some(current.replacen(after, before, 1));
            } else {
                failure = Some("it changed after the agent's edit".to_string());
                break;
            }
        }
        if let Some(reason) = failure {
            report.skipped.push((path.to_path_buf(), reason));
            continue;
        }
        let written = match &state {
            Some(content) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(path, content)
            }
            None => match std::fs::remove_file(path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                Ok(()) | Err(_) => Ok(()),
            },
        };
        match written {
            Ok(()) => report.restored.push(path.to_path_buf()),
            Err(error) => report.skipped.push((path.to_path_buf(), error.to_string())),
        }
    }
    report
}

fn completion_message(label: &str, reports: &[RestoreReport]) -> (bool, String) {
    let restored: usize = reports.iter().map(|report| report.restored.len()).sum();
    let skipped: Vec<&(PathBuf, String)> = reports
        .iter()
        .flat_map(|report| report.skipped.iter())
        .collect();
    let ran_commands = reports.iter().any(|report| report.ran_commands);
    let mut message = match restored {
        0 => format!("Undo restored no files for \"{label}\"."),
        1 => format!("Undo restored 1 file for \"{label}\"."),
        restored => format!("Undo restored {restored} files for \"{label}\"."),
    };
    for (path, reason) in &skipped {
        message.push_str(&format!("\nSkipped {}: {reason}.", path.display()));
    }
    if ran_commands {
        message.push_str(
            "\nThe agent also ran commands; changes made by commands were not reported as edits and are not undone.",
        );
    }
    message.push_str("\nNote: the agent is not aware that files have changed.");
    (
        skipped.is_empty() && (restored > 0 || !ran_commands),
        message,
    )
}

async fn send_started(event_tx: &mpsc::Sender<BackendEvent>, id: &str) {
    let _ = event_tx
        .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Undo(
            UndoEvent::Started {
                operation_id: id.to_string(),
                message: Some("Undo in progress...".to_string()),
            },
        ))))
        .await;
}

async fn send_completed(
    event_tx: &mpsc::Sender<BackendEvent>,
    id: &str,
    success: bool,
    message: Option<String>,
) {
    let _ = event_tx
        .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Undo(
            UndoEvent::Completed {
                operation_id: id.to_string(),
                success,
                message,
            },
        ))))
        .await;
}

/// Undo through the journal at the given display index (0 = the most recent
/// undoable turn), restoring every newer turn on the way.
///
/// Emits `UndoStarted` and `UndoCompleted` events on the provided channel.
pub async fn handle_undo_to(
    event_tx: &mpsc::Sender<BackendEvent>,
    id: &str,
    journal: &EditJournal,
    index: i64,
) {
    send_started(event_tx, id).await;
    let (success, message) = match journal.take_through_index(index) {
        Err(RestoreError::Empty) => {
            warn!("Undo requested but no edits were recorded");
            (
                false,
                "Nothing to undo: no file edits were reported this session.".to_string(),
            )
        }
        Err(RestoreError::OutOfBounds) => {
            warn!("Undo requested with out-of-bounds index {index}");
            (false, format!("Invalid undo index: {index}"))
        }
        Ok(turns) => {
            let label = turns
                .last()
                .map(|turn| turn.label.clone())
                .unwrap_or_default();
            match tokio::task::spawn_blocking(move || {
                turns.iter().map(restore_turn).collect::<Vec<_>>()
            })
            .await
            {
                Ok(reports) => {
                    info!(turns = reports.len(), "Undo restored recorded edits");
                    completion_message(&label, &reports)
                }
                Err(error) => (false, format!("Undo failed: {error}")),
            }
        }
    };
    send_completed(event_tx, id, success, Some(message)).await;
}

/// Undo the most recent undoable turn.
pub async fn handle_undo(event_tx: &mpsc::Sender<BackendEvent>, id: &str, journal: &EditJournal) {
    handle_undo_to(event_tx, id, journal, 0).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalized::FileChange;
    use crate::normalized::ToolPhase;
    use pretty_assertions::assert_eq;

    fn diff_snapshot(call_id: &str, path: &Path, old: Option<&str>, new: &str) -> ToolSnapshot {
        ToolSnapshot {
            call_id: call_id.to_string(),
            title: "edit".to_string(),
            kind: ToolKind::Edit,
            phase: ToolPhase::Completed,
            locations: Vec::new(),
            invocation: None,
            artifacts: vec![Artifact::Diff(FileChange {
                path: path.to_path_buf(),
                old_text: old.map(str::to_string),
                new_text: new.to_string(),
            })],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        }
    }

    fn command_snapshot(call_id: &str) -> ToolSnapshot {
        ToolSnapshot {
            kind: ToolKind::Execute,
            artifacts: Vec::new(),
            ..diff_snapshot(call_id, Path::new("unused"), None, "")
        }
    }

    fn restore_latest(journal: &EditJournal) -> RestoreReport {
        let turns = journal.take_through_index(0).ok().unwrap();
        turns.iter().map(restore_turn).next().unwrap()
    }

    #[test]
    fn whole_file_edit_is_restored_and_popped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "before").unwrap();
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("edit a".to_string());
        journal.record(
            &diff_snapshot("c1", &file, Some("before"), "after"),
            dir.path(),
            None,
        );
        std::fs::write(&file, "after").unwrap();

        assert_eq!(
            journal.list(),
            vec![UndoSnapshot {
                index: 0,
                short_id: "1 file".to_string(),
                label: "edit a".to_string(),
            }]
        );
        let report = restore_latest(&journal);

        assert_eq!(report.restored, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
        assert_eq!(journal.list(), Vec::new());
    }

    #[test]
    fn fragment_diffs_are_replaced_inside_the_current_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo-new\nthree\n").unwrap();
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("edit".to_string());
        journal.record(
            &diff_snapshot("c1", &file, Some("two"), "two-new"),
            dir.path(),
            None,
        );

        let report = restore_latest(&journal);

        assert_eq!(report.restored, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\nthree\n");
    }

    #[test]
    fn created_file_is_removed_when_it_did_not_exist_before() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.txt");
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("create".to_string());
        journal.record(&diff_snapshot("c1", &file, None, "hello"), dir.path(), None);
        std::fs::write(&file, "hello").unwrap();

        let report = restore_latest(&journal);

        assert_eq!(report.restored, vec![file.clone()]);
        assert!(!file.exists());
    }

    #[test]
    fn overwrite_without_reported_original_is_captured_before_it_lands() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "precious").unwrap();
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("write".to_string());
        journal.record(
            &diff_snapshot("c1", &file, None, "replacement"),
            dir.path(),
            None,
        );
        std::fs::write(&file, "replacement").unwrap();

        let report = restore_latest(&journal);

        assert_eq!(report.restored, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "precious");
    }

    #[test]
    fn overwrite_seen_after_it_landed_is_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "replacement").unwrap();
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("write".to_string());
        journal.record(
            &diff_snapshot("c1", &file, None, "replacement"),
            dir.path(),
            None,
        );

        let report = restore_latest(&journal);

        assert_eq!(report.restored, Vec::<PathBuf>::new());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "replacement");
    }

    #[test]
    fn later_changes_by_the_user_are_not_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("edit".to_string());
        journal.record(
            &diff_snapshot("c1", &file, Some("before"), "after"),
            dir.path(),
            None,
        );
        std::fs::write(&file, "edited by the user").unwrap();

        let report = restore_latest(&journal);

        assert_eq!(report.restored, Vec::<PathBuf>::new());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "edited by the user"
        );
    }

    #[test]
    fn repeated_edits_to_one_path_unwind_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("two edits".to_string());
        journal.record(
            &diff_snapshot("c1", &file, Some("v0"), "v1"),
            dir.path(),
            None,
        );
        journal.record(
            &diff_snapshot("c2", &file, Some("v1"), "v2"),
            dir.path(),
            None,
        );
        std::fs::write(&file, "v2").unwrap();

        let report = restore_latest(&journal);

        assert_eq!(report.restored, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v0");
    }

    #[test]
    fn repeated_snapshots_of_one_call_are_recorded_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("edit".to_string());
        for _ in 0..3 {
            journal.record(
                &diff_snapshot("c1", &file, Some("v0"), "v1"),
                dir.path(),
                None,
            );
        }

        let turns = journal.lock();
        assert_eq!(turns[0].calls.len(), 1);
        assert_eq!(turns[0].calls[0].edits.len(), 1);
    }

    #[test]
    fn undo_to_restores_newer_turns_too_and_skips_turns_without_edits() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        std::fs::write(&first, "1-new").unwrap();
        std::fs::write(&second, "2-new").unwrap();
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("first".to_string());
        journal.record(
            &diff_snapshot("c1", &first, Some("1-old"), "1-new"),
            dir.path(),
            None,
        );
        journal.begin_turn("chat only".to_string());
        journal.begin_turn("second".to_string());
        journal.record(
            &diff_snapshot("c2", &second, Some("2-old"), "2-new"),
            dir.path(),
            None,
        );

        let labels: Vec<String> = journal.list().into_iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["second".to_string(), "first".to_string()]);

        let turns = journal.take_through_index(1).ok().unwrap();
        let restored = turns.iter().map(restore_turn).count();

        // The chat-only turn between the two is popped with them.
        assert_eq!(restored, 3);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "1-old");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "2-old");
        assert!(matches!(
            journal.take_through_index(0),
            Err(RestoreError::Empty)
        ));
    }

    #[test]
    fn out_of_bounds_index_is_rejected_and_keeps_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("edit".to_string());
        journal.record(
            &diff_snapshot("c1", &dir.path().join("a"), Some("x"), "y"),
            dir.path(),
            None,
        );

        assert!(matches!(
            journal.take_through_index(1),
            Err(RestoreError::OutOfBounds)
        ));
        assert!(matches!(
            journal.take_through_index(-1),
            Err(RestoreError::OutOfBounds)
        ));
        assert_eq!(journal.list().len(), 1);
    }

    #[test]
    fn commands_are_flagged_and_listed() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(EditJournal::new());
        journal.begin_turn("run it".to_string());
        journal.record(&command_snapshot("c1"), dir.path(), None);

        assert_eq!(
            journal.list(),
            vec![UndoSnapshot {
                index: 0,
                short_id: "commands only".to_string(),
                label: "run it".to_string(),
            }]
        );
        let report = restore_latest(&journal);
        let (success, message) = completion_message("run it", &[report]);
        assert!(!success);
        assert!(message.contains("not undone"), "{message}");
    }

    #[test]
    fn snapshots_without_an_open_turn_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(EditJournal::new());
        journal.record(
            &diff_snapshot("c1", &dir.path().join("a"), Some("x"), "y"),
            dir.path(),
            None,
        );
        assert_eq!(journal.list(), Vec::new());
    }
}
