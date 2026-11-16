//! The agents built into this binary, registered so a `native` distribution can start them.

/// Make every built-in agent this build has available to the ACP host.
pub fn register() {
    #[cfg(feature = "native-codex")]
    register_codex();
    #[cfg(feature = "native-claude")]
    register_claude();
}

#[cfg(feature = "native-claude")]
fn register_claude() {
    use std::sync::Arc;

    nori_acp_host::in_process::register_in_process_agent(
        "claude",
        Arc::new(|spec, pipe| {
            Box::pin(async move {
                let mut options = claude_code_acp::InProcessOptions::default();
                for pair in &spec.config {
                    match pair.split_once('=') {
                        Some(("cli_path", path)) => options.cli_path = Some(path.into()),
                        Some(("model", model)) => options.model = Some(model.to_owned()),
                        _ => {}
                    }
                }
                claude_code_acp::serve_in_process(pipe, options)
                    .await
                    .map_err(std::io::Error::other)
            })
        }),
    );
}

#[cfg(feature = "native-codex")]
fn register_codex() {
    use std::sync::Arc;

    nori_acp_host::in_process::register_in_process_agent(
        "codex",
        Arc::new(|spec, pipe| {
            Box::pin(async move {
                codex_acp::serve_over_pipe(
                    pipe,
                    codex_acp::InProcessOptions {
                        codex_home: spec.home,
                        config_overrides: spec.config,
                        codex_linux_sandbox_exe: None,
                        ignore_host_config: true,
                    },
                )
                .await
            })
        }),
    );
}
