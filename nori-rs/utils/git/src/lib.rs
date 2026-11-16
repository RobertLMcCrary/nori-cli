//! Git worktrees for auto-worktree sessions, managed in-process with `gix`.

mod errors;
mod worktree;

pub use errors::GitToolingError;
pub use worktree::WorktreeInfo;
pub use worktree::create_detached_worktree;
pub use worktree::create_worktree;
pub use worktree::ensure_gitignore_entry;
pub use worktree::generate_worktree_branch_name;
pub use worktree::list_worktrees;
pub use worktree::rename_worktree_branch;
pub use worktree::summary_to_branch_name;
