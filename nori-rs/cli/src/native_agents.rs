//! The agents built into this binary, registered so a `native` distribution can start them.

/// Make every built-in agent this build has available to the ACP host.
pub fn register() {
    #[cfg(feature = "native-codex")]
    register_codex();
    #[cfg(feature = "native-claude")]
    register_claude();
    #[cfg(feature = "native-opencode")]
    register_opencode();
    #[cfg(feature = "native-gemini")]
    register_gemini();
}

#[cfg(feature = "native-gemini")]
fn register_gemini() {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;

    nori_acp_host::in_process::register_in_process_agent(
        "gemini",
        Arc::new(|spec, pipe| {
            Box::pin(async move {
                let mut node = PathBuf::from("node");
                let mut module = std::env::var_os("NORI_GEMINI_CORE_MODULE").map(PathBuf::from);
                let mut environment = spec
                    .environment
                    .into_iter()
                    .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                    .collect::<std::collections::HashMap<_, _>>();
                if let Some(home) = spec.home {
                    environment.insert("HOME".into(), home.into());
                }
                if let Some(path) = environment.get(&OsString::from("NORI_GEMINI_CORE_MODULE")) {
                    module = Some(path.into());
                }
                for option in &spec.config {
                    if let Some(path) = option.strip_prefix("node_path=") {
                        node = PathBuf::from(path);
                    } else if let Some(path) = option.strip_prefix("core_path=") {
                        module = Some(PathBuf::from(path));
                    } else if let Some(auth) = option.strip_prefix("auth=") {
                        if !matches!(auth, "gemini" | "vertex") {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "native Gemini supports auth=gemini or auth=vertex; use the official CLI for Google account login",
                            ));
                        }
                        environment.insert("NORI_GEMINI_AUTH".into(), auth.into());
                    } else {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("unsupported Gemini native option: {option}"),
                        ));
                    }
                }
                let node = which::which(&node).map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("Node executable not found: {error}"),
                    )
                })?;
                let module = module.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "set NORI_GEMINI_CORE_MODULE or core_path to the official Gemini Core entry point"))?.canonicalize()?;
                nori_gemini_acp::serve_over_pipe(pipe, node, module, environment).await
            })
        }),
    );
}

#[cfg(feature = "native-opencode")]
fn register_opencode() {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;

    nori_acp_host::in_process::register_in_process_agent(
        "opencode",
        Arc::new(|spec, pipe| {
            Box::pin(async move {
                let mut executable = PathBuf::from("opencode");
                for option in &spec.config {
                    if let Some(path) = option.strip_prefix("cli_path=") {
                        executable = PathBuf::from(path);
                    } else {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("unsupported OpenCode native option: {option}"),
                        ));
                    }
                }
                let executable = which::which(&executable).map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("OpenCode executable not found: {error}"),
                    )
                })?;
                let mut environment = spec
                    .environment
                    .into_iter()
                    .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                    .collect::<std::collections::HashMap<_, _>>();
                if let Some(home) = spec.home {
                    for (key, directory) in [
                        ("XDG_CONFIG_HOME", "config"),
                        ("XDG_DATA_HOME", "data"),
                        ("XDG_STATE_HOME", "state"),
                        ("XDG_CACHE_HOME", "cache"),
                    ] {
                        environment
                            .entry(key.into())
                            .or_insert_with(|| home.join(directory).into());
                    }
                }
                nori_opencode_acp::serve_over_pipe(pipe, executable, environment).await
            })
        }),
    );
}

#[cfg(feature = "native-claude")]
fn register_claude() {
    use std::sync::Arc;

    nori_acp_host::in_process::register_in_process_agent(
        "claude",
        Arc::new(|spec, pipe| {
            Box::pin(async move {
                claude_code_acp::serve_in_process(pipe, claude_options(&spec))
                    .await
                    .map_err(std::io::Error::other)
            })
        }),
    );
    // Claude Code takes about a second to start, so a session is started before it is needed.
    nori_acp_host::in_process::register_in_process_prewarm(
        "claude",
        Arc::new(|spec, cwd, mcp_servers| {
            claude_code_acp::prewarm(claude_options(&spec), cwd, mcp_servers);
        }),
    );
}

/// The driver's options for `spec`. Sessions get Nori's own tools directly, with no MCP server
/// between them.
#[cfg(feature = "native-claude")]
fn claude_options(
    spec: &nori_acp_host::in_process::InProcessSpec,
) -> claude_code_acp::InProcessOptions {
    let mut options = claude_code_acp::InProcessOptions {
        host_tools: true,
        ..Default::default()
    };
    for pair in &spec.config {
        match pair.split_once('=') {
            Some(("cli_path", path)) => options.cli_path = Some(path.into()),
            Some(("model", model)) => options.model = Some(model.to_owned()),
            _ => {}
        }
    }
    options
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
                        host_tools: true,
                    },
                )
                .await
            })
        }),
    );
}
