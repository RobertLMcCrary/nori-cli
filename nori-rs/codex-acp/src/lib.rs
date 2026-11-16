//! Codex ACP - An Agent Client Protocol implementation for Codex.
#![deny(clippy::print_stdout, clippy::print_stderr)]

use agent_client_protocol::{Agent, ByteStreams, ConnectTo};
use codex_core::config::{ConfigBuilder, ConfigOverrides};
use codex_utils_cli::CliConfigOverrides;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing_subscriber::EnvFilter;

mod codex_agent;
mod thread;

/// Run the Codex ACP agent as a standalone process, over stdio.
///
/// This sets up an ACP agent that communicates over stdio, bridging
/// the ACP protocol with the existing codex-rs infrastructure.
///
/// # Errors
///
/// If unable to parse the config or start the program.
pub async fn run_main(
    codex_linux_sandbox_exe: Option<PathBuf>,
    cli_config_overrides: CliConfigOverrides,
) -> std::io::Result<()> {
    // Install a simple subscriber so `tracing` output is visible.
    // Users can control the log level with `RUST_LOG`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let agent = load_agent(codex_linux_sandbox_exe, None, false, cli_config_overrides).await?;

    let stdin = tokio::io::stdin().compat();
    let stdout = tokio::io::stdout().compat_write();

    agent
        .serve(ByteStreams::new(stdout, stdin))
        .await
        .map_err(|e| std::io::Error::other(format!("ACP error: {e}")))?;

    Ok(())
}

/// What an in-process Codex agent is configured with.
#[derive(Debug, Clone, Default)]
pub struct InProcessOptions {
    /// Where this agent's Codex state and login live. Passed explicitly because a process-wide
    /// `CODEX_HOME` cannot tell two accounts apart. `None` falls back to the environment.
    pub codex_home: Option<PathBuf>,
    /// `key=value` pairs as `codex -c` takes them, e.g. `model="o3"`.
    pub config_overrides: Vec<String>,
    /// The Linux sandbox helper, where the platform needs one.
    pub codex_linux_sandbox_exe: Option<PathBuf>,
    /// Do not read the host-wide configuration (`/etc/codex/config.toml` and managed policy).
    /// That file is written for whichever Codex the machine has installed, and the one linked
    /// into this process may be older and fail to parse it.
    pub ignore_host_config: bool,
}

/// Run the Codex ACP agent inside the calling process, over `transport`.
///
/// This is how Nori embeds Codex: its ACP client sits on the other end of an in-memory pipe, so
/// no process is spawned. Unlike [`run_main`] it leaves the global `tracing` subscriber to the
/// host.
///
/// # Errors
///
/// If the configuration cannot be loaded or the connection fails.
pub async fn serve_in_process(
    transport: impl ConnectTo<Agent> + 'static,
    options: InProcessOptions,
) -> std::io::Result<()> {
    let overrides = CliConfigOverrides {
        raw_overrides: options.config_overrides,
    };
    let agent = load_agent(
        options.codex_linux_sandbox_exe,
        options.codex_home,
        options.ignore_host_config,
        overrides,
    )
    .await?;
    agent
        .serve(transport)
        .await
        .map_err(|e| std::io::Error::other(format!("ACP error: {e}")))
}

/// [`serve_in_process`] over one end of an in-memory pipe. The host keeps the other end, so it
/// needs none of this crate's protocol types, only `tokio`.
///
/// # Errors
///
/// If the configuration cannot be loaded or the connection fails.
pub async fn serve_over_pipe(
    pipe: tokio::io::DuplexStream,
    options: InProcessOptions,
) -> std::io::Result<()> {
    let (read, write) = tokio::io::split(pipe);
    serve_in_process(
        ByteStreams::new(write.compat_write(), read.compat()),
        options,
    )
    .await
}

/// Load Codex's configuration and build the agent over it.
async fn load_agent(
    codex_linux_sandbox_exe: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    ignore_host_config: bool,
    cli_config_overrides: CliConfigOverrides,
) -> std::io::Result<Arc<codex_agent::CodexAgent>> {
    // Parse CLI overrides and load configuration
    let cli_kv_overrides = cli_config_overrides.parse_overrides().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("error parsing -c overrides: {e}"),
        )
    })?;

    let config_overrides = ConfigOverrides {
        codex_linux_sandbox_exe: codex_linux_sandbox_exe.clone(),
        ..ConfigOverrides::default()
    };

    let mut builder = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .harness_overrides(config_overrides);
    if ignore_host_config {
        // Point the host-wide files at a path that does not exist, so they load as empty.
        let nowhere = std::env::temp_dir().join("nori-codex-no-host-config");
        builder = builder.loader_overrides(codex_config::LoaderOverrides {
            system_config_path: Some(nowhere.join("config.toml")),
            system_requirements_path: Some(nowhere.join("requirements.toml")),
            managed_config_path: Some(nowhere.join("managed_config.toml")),
            ignore_managed_requirements: true,
            ..Default::default()
        });
    }
    if let Some(codex_home) = codex_home {
        builder = builder.codex_home(codex_home);
    }
    let config = builder.build().await.map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("error loading config: {e}"),
        )
    })?;
    // Apply residency requirement so the HTTP client sends the
    // x-openai-internal-codex-residency header on all requests.
    codex_login::default_client::set_default_client_residency_requirement(
        config.enforce_residency.value(),
    );

    Ok(Arc::new(
        codex_agent::CodexAgent::new(config, codex_linux_sandbox_exe).await?,
    ))
}
