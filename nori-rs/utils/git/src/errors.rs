use std::path::PathBuf;

use thiserror::Error;

/// Errors returned while managing git worktrees.
#[derive(Debug, Error)]
pub enum GitToolingError {
    #[error("{path:?} is not a git repository")]
    NotAGitRepository { path: PathBuf },
    #[error("git: {0}")]
    Gix(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
