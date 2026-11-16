//! TranscriptRecorder - Records transcript entries for a session.
//!
//! Uses async channel for non-blocking writes (same pattern as core RolloutRecorder).

use std::io;
use std::io::Error as IoError;
use std::path::Path;
use std::path::PathBuf;

use serde_json;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use super::BY_PROJECT_DIR;
use super::PROJECT_METADATA_FILE;
use super::SESSIONS_DIR;
use super::TRANSCRIPTS_DIR;
use super::project::ProjectId;
use super::project::compute_project_id;
use super::types::Attachment;
use super::types::GitInfo;
use super::types::SessionEventEntry;
use super::types::SessionMetaEntry;
use super::types::TranscriptEntry;
use super::types::TranscriptLine;
use super::types::UserEntry;
use super::types::now_iso8601;

/// Commands sent to the background writer task.
enum TranscriptCmd {
    Write(Box<TranscriptEntry>),
    Flush { ack: oneshot::Sender<()> },
    Shutdown { ack: oneshot::Sender<()> },
}

/// Records transcript entries for a session.
/// Uses async channel for non-blocking writes (same pattern as core RolloutRecorder).
#[derive(Clone)]
pub struct TranscriptRecorder {
    tx: Sender<TranscriptCmd>,
    session_id: String,
    project_id: String,
    transcript_path: PathBuf,
    /// It appends to a session recorded before, which already holds the history a resume
    /// replays.
    continues_recorded_session: bool,
}

impl TranscriptRecorder {
    /// Initialize for a new session.
    ///
    /// - Detects project from cwd (git root or cwd path)
    /// - Creates project directory if needed
    /// - Opens new session JSONL file
    /// - Writes SessionMeta as first entry
    pub async fn new(
        nori_home: &Path,
        cwd: &Path,
        agent: Option<String>,
        cli_version: &str,
        acp_session_id: Option<String>,
    ) -> io::Result<Self> {
        // Both only need the cwd, and each waits on git, so they run together.
        let (project_id_info, git_info) =
            tokio::join!(compute_project_id(cwd), collect_git_info(cwd));
        let project_id_info = project_id_info?;

        // Create session ID (UUID)
        let session_id = generate_session_id();

        // Create directory structure
        let project_dir = nori_home
            .join(TRANSCRIPTS_DIR)
            .join(BY_PROJECT_DIR)
            .join(&project_id_info.id);
        let sessions_dir = project_dir.join(SESSIONS_DIR);
        tokio::fs::create_dir_all(&sessions_dir).await?;

        // Write/update project metadata
        let project_meta_path = project_dir.join(PROJECT_METADATA_FILE);
        write_project_metadata(&project_meta_path, &project_id_info).await?;

        // Create session transcript file
        let transcript_path = sessions_dir.join(format!("{session_id}.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&transcript_path)
            .await?;

        // Set file permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&transcript_path, perms).await?;
        }

        // Create channel and spawn writer task
        let (tx, rx) = mpsc::channel::<TranscriptCmd>(256);

        // Create session metadata
        let session_meta = SessionMetaEntry {
            session_id: session_id.clone(),
            project_id: project_id_info.id.clone(),
            started_at: now_iso8601(),
            cwd: cwd.to_path_buf(),
            agent,
            cli_version: cli_version.to_string(),
            git: git_info,
            acp_session_id,
            forked_from: None,
        };

        // Spawn background writer
        tokio::spawn(transcript_writer(file, rx, session_meta));

        Ok(Self {
            tx,
            session_id,
            project_id: project_id_info.id,
            transcript_path,
            continues_recorded_session: false,
        })
    }

    /// Initialize a forked session seeded from a parent transcript.
    ///
    /// Mints a fresh session UUID, opens a new transcript file, and writes the
    /// `SessionMeta` (recording `forked_from` lineage and the new ACP session
    /// id) followed by `seed_entries` in a single batched write before
    /// returning, so the fork does not incur a per-line fsync storm on large
    /// parents.
    pub async fn new_forked(
        nori_home: &Path,
        cwd: &Path,
        agent: Option<String>,
        cli_version: &str,
        new_acp_session_id: String,
        forked_from: String,
        seed_entries: Vec<TranscriptEntry>,
    ) -> io::Result<Self> {
        let project_id_info = compute_project_id(cwd).await?;
        let session_id = generate_session_id();

        let project_dir = nori_home
            .join(TRANSCRIPTS_DIR)
            .join(BY_PROJECT_DIR)
            .join(&project_id_info.id);
        let sessions_dir = project_dir.join(SESSIONS_DIR);
        tokio::fs::create_dir_all(&sessions_dir).await?;

        let project_meta_path = project_dir.join(PROJECT_METADATA_FILE);
        write_project_metadata(&project_meta_path, &project_id_info).await?;

        let transcript_path = sessions_dir.join(format!("{session_id}.jsonl"));
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&transcript_path)
            .await?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&transcript_path, perms).await?;
        }

        let git_info = collect_git_info(cwd).await;
        let session_meta = SessionMetaEntry {
            session_id: session_id.clone(),
            project_id: project_id_info.id.clone(),
            started_at: now_iso8601(),
            cwd: cwd.to_path_buf(),
            agent,
            cli_version: cli_version.to_string(),
            git: git_info,
            acp_session_id: Some(new_acp_session_id),
            forked_from: Some(forked_from),
        };

        // Batch the metadata line plus every seeded entry into a single write.
        let mut batch = serialize_line(&TranscriptLine::new(TranscriptEntry::SessionMeta(
            session_meta,
        )))?;
        for entry in seed_entries {
            batch.push_str(&serialize_line(&TranscriptLine::new(entry))?);
        }
        file.write_all(batch.as_bytes()).await?;
        file.flush().await?;

        let (tx, rx) = mpsc::channel::<TranscriptCmd>(256);
        tokio::spawn(run_writer_loop(file, rx));

        Ok(Self {
            tx,
            session_id,
            project_id: project_id_info.id,
            transcript_path,
            continues_recorded_session: false,
        })
    }

    /// Continue recording the session that `transcript_path` holds, whose metadata is `meta`.
    ///
    /// What is recorded from now on is appended to that file under the session's own id, so a
    /// resumed session keeps one transcript instead of a new copy of it per resume.
    pub async fn continue_session(
        transcript_path: PathBuf,
        meta: &SessionMetaEntry,
    ) -> io::Result<Self> {
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&transcript_path)
            .await?;
        // A session that stopped in the middle of a line (a crash, a full disk) has that line
        // ended first, so that what is appended starts on a line of its own.
        if file.metadata().await?.len() > 0 {
            file.seek(io::SeekFrom::End(-1)).await?;
            let mut last = [0];
            file.read_exact(&mut last).await?;
            if last != *b"\n" {
                file.write_all(b"\n").await?;
            }
        }
        let (tx, rx) = mpsc::channel::<TranscriptCmd>(256);
        tokio::spawn(run_writer_loop(file, rx));
        Ok(Self {
            tx,
            session_id: meta.session_id.clone(),
            project_id: meta.project_id.clone(),
            transcript_path,
            continues_recorded_session: true,
        })
    }

    /// Whether this recorder appends to a session recorded before (see [`Self::continue_session`]).
    pub fn continues_recorded_session(&self) -> bool {
        self.continues_recorded_session
    }

    /// Record a user message.
    pub async fn record_user_message(
        &self,
        id: &str,
        content: &str,
        attachments: Vec<Attachment>,
    ) -> io::Result<()> {
        let entry = TranscriptEntry::User(UserEntry {
            id: id.to_string(),
            content: content.to_string(),
            attachments,
        });
        self.send_entry(entry).await
    }

    /// Record the exact public event delivered across the Harness boundary.
    pub(crate) async fn record_session_event(&self, event: &SessionEventEntry) -> io::Result<()> {
        self.send_entry(TranscriptEntry::SessionEvent(event.clone()))
            .await
    }

    /// Flush all pending writes.
    pub async fn flush(&self) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(TranscriptCmd::Flush { ack: tx })
            .await
            .map_err(|e| IoError::other(format!("failed to queue transcript flush: {e}")))?;
        rx.await
            .map_err(|e| IoError::other(format!("failed waiting for transcript flush: {e}")))
    }

    /// Graceful shutdown.
    pub async fn shutdown(&self) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(TranscriptCmd::Shutdown { ack: tx })
            .await
            .map_err(|e| IoError::other(format!("failed to queue transcript shutdown: {e}")))?;
        rx.await
            .map_err(|e| IoError::other(format!("failed waiting for transcript shutdown: {e}")))
    }

    /// Get the path to this session's transcript file.
    pub fn transcript_path(&self) -> &Path {
        &self.transcript_path
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the project ID.
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Send an entry to the background writer.
    async fn send_entry(&self, entry: TranscriptEntry) -> io::Result<()> {
        self.tx
            .send(TranscriptCmd::Write(Box::new(entry)))
            .await
            .map_err(|e| IoError::other(format!("failed to queue transcript entry: {e}")))
    }
}

/// Background writer task that processes commands and writes to file.
async fn transcript_writer(
    mut file: File,
    rx: mpsc::Receiver<TranscriptCmd>,
    session_meta: SessionMetaEntry,
) -> io::Result<()> {
    // Write session metadata as the first line
    let meta_entry = TranscriptEntry::SessionMeta(session_meta);
    let line = TranscriptLine::new(meta_entry);
    write_line(&mut file, &line).await?;

    run_writer_loop(file, rx).await
}

/// Process writer commands against an already-opened, already-seeded file.
async fn run_writer_loop(mut file: File, mut rx: mpsc::Receiver<TranscriptCmd>) -> io::Result<()> {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            TranscriptCmd::Write(entry) => {
                let line = TranscriptLine::new(*entry);
                write_line(&mut file, &line).await?;
            }
            TranscriptCmd::Flush { ack } => {
                if let Err(e) = file.flush().await {
                    let _ = ack.send(());
                    return Err(e);
                }
                let _ = ack.send(());
            }
            TranscriptCmd::Shutdown { ack } => {
                let _ = file.flush().await;
                let _ = ack.send(());
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Serialize a transcript line into its newline-terminated JSONL form.
fn serialize_line(line: &TranscriptLine) -> io::Result<String> {
    let mut json = serde_json::to_string(line)?;
    json.push('\n');
    Ok(json)
}

/// Write a single JSONL line to the file.
async fn write_line(file: &mut File, line: &TranscriptLine) -> io::Result<()> {
    let json = serialize_line(line)?;
    file.write_all(json.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

/// Generate a UUID for the session ID.
fn generate_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Write project metadata to project.json.
async fn write_project_metadata(path: &Path, project_id: &ProjectId) -> io::Result<()> {
    use serde_json::json;

    let now = now_iso8601();

    let metadata = json!({
        "id": project_id.id,
        "name": project_id.name,
        "git_remote": project_id.git_remote,
        "git_root": project_id.git_root,
        "cwd": project_id.cwd,
        "created_at": now,
        "updated_at": now,
    });

    let content = serde_json::to_string_pretty(&metadata)?;
    tokio::fs::write(path, content).await
}

/// Collect git info for session metadata.
async fn collect_git_info(cwd: &Path) -> Option<GitInfo> {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let repo = nori_config::GitRepo::discover(&cwd)?;
        Some(GitInfo {
            branch: repo.branch(),
            commit_hash: repo.head_commit(),
        })
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_transcript_recorder_creates_directory_structure() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let cwd = temp_dir.path();

        let recorder = TranscriptRecorder::new(
            nori_home,
            cwd,
            Some("claude-code".to_string()),
            "0.1.0",
            None,
        )
        .await
        .unwrap();

        // Verify directory structure was created
        let project_dir = nori_home
            .join(TRANSCRIPTS_DIR)
            .join(BY_PROJECT_DIR)
            .join(recorder.project_id());
        assert!(project_dir.exists());
        assert!(project_dir.join(SESSIONS_DIR).exists());
        assert!(project_dir.join(PROJECT_METADATA_FILE).exists());

        // Verify transcript file was created
        assert!(recorder.transcript_path().exists());

        recorder.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_resumed_session_goes_on_recording_into_its_own_transcript() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let first = TranscriptRecorder::new(nori_home, nori_home, None, "0.1.0", None)
            .await
            .unwrap();
        first
            .record_user_message("msg-1", "first turn", vec![])
            .await
            .unwrap();
        first.shutdown().await.unwrap();
        // The session stopped in the middle of writing a line.
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(first.transcript_path())
            .await
            .unwrap()
            .write_all(b"{\"ts\":\"2026")
            .await
            .unwrap();
        let loader = super::super::TranscriptLoader::new(nori_home.to_path_buf());
        let transcript = loader
            .load_transcript(first.project_id(), first.session_id())
            .await
            .unwrap();

        let resumed = TranscriptRecorder::continue_session(
            first.transcript_path().to_path_buf(),
            &transcript.meta,
        )
        .await
        .unwrap();
        resumed
            .record_user_message("msg-2", "second turn", vec![])
            .await
            .unwrap();
        resumed.shutdown().await.unwrap();

        assert!(resumed.continues_recorded_session());
        assert_eq!(resumed.session_id(), first.session_id());
        assert_eq!(resumed.transcript_path(), first.transcript_path());
        let sessions = std::fs::read_dir(first.transcript_path().parent().unwrap())
            .unwrap()
            .count();
        assert_eq!(sessions, 1);
        let reloaded = loader
            .load_transcript(first.project_id(), first.session_id())
            .await
            .unwrap();
        let turns: Vec<&str> = reloaded
            .entries
            .iter()
            .filter_map(|line| match &line.entry {
                TranscriptEntry::User(user) => Some(user.content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(turns, vec!["first turn", "second turn"]);
    }

    #[tokio::test]
    async fn test_transcript_recorder_writes_session_meta() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let cwd = temp_dir.path();

        let recorder = TranscriptRecorder::new(
            nori_home,
            cwd,
            Some("claude-code".to_string()),
            "0.1.0",
            None,
        )
        .await
        .unwrap();

        // Give the writer a moment to write the session meta
        recorder.flush().await.unwrap();
        recorder.shutdown().await.unwrap();

        // Read the transcript file
        let content = tokio::fs::read_to_string(recorder.transcript_path())
            .await
            .unwrap();
        let lines: Vec<&str> = content.lines().collect();

        assert!(!lines.is_empty());

        // First line should be session_meta
        let first_line: TranscriptLine = serde_json::from_str(lines[0]).unwrap();
        match first_line.entry {
            TranscriptEntry::SessionMeta(meta) => {
                assert_eq!(meta.session_id, recorder.session_id());
                assert_eq!(meta.project_id, recorder.project_id());
                assert_eq!(meta.cli_version, "0.1.0");
                assert_eq!(meta.agent, Some("claude-code".to_string()));
            }
            _ => panic!("Expected SessionMeta entry"),
        }
    }

    #[tokio::test]
    async fn test_transcript_recorder_records_user_message() {
        let temp_dir = TempDir::new().unwrap();
        let nori_home = temp_dir.path();
        let cwd = temp_dir.path();

        let recorder = TranscriptRecorder::new(nori_home, cwd, None, "0.1.0", None)
            .await
            .unwrap();

        recorder
            .record_user_message("msg-001", "Hello, world!", vec![])
            .await
            .unwrap();
        recorder.flush().await.unwrap();
        recorder.shutdown().await.unwrap();

        // Read the transcript file
        let content = tokio::fs::read_to_string(recorder.transcript_path())
            .await
            .unwrap();
        let lines: Vec<&str> = content.lines().collect();

        assert_eq!(lines.len(), 2); // SessionMeta + User message

        let user_line: TranscriptLine = serde_json::from_str(lines[1]).unwrap();
        match user_line.entry {
            TranscriptEntry::User(user) => {
                assert_eq!(user.id, "msg-001");
                assert_eq!(user.content, "Hello, world!");
            }
            _ => panic!("Expected User entry"),
        }
    }
}
