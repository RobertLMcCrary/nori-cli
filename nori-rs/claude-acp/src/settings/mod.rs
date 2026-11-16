//! Settings management
//!
//! Loads and merges settings from multiple sources:
//! - User settings: `~/.claude/settings.json`
//! - Project settings: `.claude/settings.json`
//! - Local settings: `.claude/settings.local.json`
//!
//! Priority: Local > Project > User

mod manager;
mod permission_checker;
mod rule;
mod watcher;

pub use manager::McpServerConfig;
pub use manager::Settings;
pub use manager::SettingsManager;
pub use permission_checker::PermissionChecker;
pub use rule::ParsedRule;
pub use rule::PermissionCheckResult;
pub use rule::PermissionDecision;
pub use rule::PermissionSettings;
pub use watcher::SettingsChangeEvent;
pub use watcher::SettingsWatcher;
pub use watcher::WatcherError;
pub use watcher::WatcherHandle;
