use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use acp::Agent;
use acp::ByteStreams;
use acp::Client;
use acp::ConnectionTo;
use acp::schema::AgentCapabilities;
use acp::schema::CancelNotification;
use acp::schema::CloseSessionRequest;
use acp::schema::CloseSessionResponse;
use acp::schema::InitializeRequest;
use acp::schema::InitializeResponse;
use acp::schema::McpCapabilities;
use acp::schema::NewSessionRequest;
use acp::schema::NewSessionResponse;
use acp::schema::PromptCapabilities;
use acp::schema::PromptRequest;
use acp::schema::SessionAdditionalDirectoriesCapabilities;
use acp::schema::SessionCapabilities;
use acp::schema::SessionCloseCapabilities;
use acp::schema::SetSessionConfigOptionRequest;
use acp::schema::SetSessionConfigOptionResponse;
use agent_client_protocol as acp;
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::engine::Engine;

struct Adapter {
    node: PathBuf,
    module: PathBuf,
    environment: HashMap<OsString, OsString>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

pub(crate) struct Session {
    pub(crate) engine: Engine,
    pub(crate) turn: Arc<Mutex<()>>,
    pub(crate) prompt_start: Mutex<()>,
    pub(crate) prompt_state: std::sync::Mutex<PromptState>,
    pub(crate) catalog: Mutex<crate::config::Catalog>,
}

#[derive(Default)]
pub(crate) struct PromptState {
    pub(crate) generation: i64,
    pub(crate) cancelled: bool,
    pub(crate) sent: bool,
}

/// Serve ACP over an in-memory pipe, launching one Gemini Core engine per session.
/// `module` is the absolute path to the installed official Core package entry point.
pub async fn serve_over_pipe(
    pipe: tokio::io::DuplexStream,
    node: PathBuf,
    module: PathBuf,
    environment: HashMap<OsString, OsString>,
) -> std::io::Result<()> {
    let agent = Arc::new(Adapter {
        node,
        module,
        environment,
        sessions: Mutex::new(HashMap::new()),
    });
    let (read, write) = tokio::io::split(pipe);
    let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
    let incoming = EofRead {
        inner: read,
        signal: Some(eof_tx),
    };
    let serving = Agent
        .builder()
        .name("nori-gemini-acp")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version).agent_capabilities(
                        AgentCapabilities::new()
                            .prompt_capabilities(
                                PromptCapabilities::new().image(true).embedded_context(true),
                            )
                            .mcp_capabilities(McpCapabilities::new().http(true).sse(true))
                            .session_capabilities(
                                SessionCapabilities::new()
                                    .close(SessionCloseCapabilities::new())
                                    .additional_directories(
                                        SessionAdditionalDirectoriesCapabilities::new(),
                                    ),
                            ),
                    ),
                )
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: NewSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    cx.spawn(async move {
                        responder.respond_with_result(agent.new_session(request).await)
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: SetSessionConfigOptionRequest,
                            responder,
                            cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    cx.spawn(async move {
                        responder.respond_with_result(agent.set_config(request).await)
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let session = agent
                        .sessions
                        .lock()
                        .await
                        .get(&request.session_id.to_string())
                        .cloned();
                    let prepared = match session {
                        Some(session) => {
                            let mut state = session.prompt_state.lock().expect("prompt state");
                            match session.turn.clone().try_lock_owned() {
                                Ok(turn) => {
                                    state.generation = state.generation.saturating_add(1);
                                    state.cancelled = false;
                                    state.sent = false;
                                    drop(state);
                                    Ok((session, turn))
                                }
                                Err(_) => Err(acp::Error::invalid_params()
                                    .data("A prompt is already running")),
                            }
                        }
                        None => Err(acp::Error::invalid_params().data("Unknown session")),
                    };
                    let client = cx.clone();
                    cx.spawn(async move {
                        let result = match prepared {
                            Ok((session, turn)) => {
                                crate::turn::run(session, request, client, turn).await
                            }
                            Err(error) => Err(error),
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: CloseSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    cx.spawn(async move {
                        if let Some(session) = agent
                            .sessions
                            .lock()
                            .await
                            .remove(&request.session_id.to_string())
                        {
                            session.engine.stop().await;
                        }
                        responder.respond(CloseSessionResponse::new())
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let agent = agent.clone();
                async move |request: CancelNotification, cx: ConnectionTo<Client>| {
                    let session = agent
                        .sessions
                        .lock()
                        .await
                        .get(&request.session_id.to_string())
                        .cloned();
                    let Some(session) = session else {
                        return Ok(());
                    };
                    let generation = {
                        let mut state = session.prompt_state.lock().expect("prompt state");
                        if session.turn.try_lock().is_ok() {
                            return Ok(());
                        }
                        state.cancelled = true;
                        state.generation
                    };
                    cx.spawn(async move {
                        let _start = session.prompt_start.lock().await;
                        let should_cancel = {
                            let state = session.prompt_state.lock().expect("prompt state");
                            state.generation == generation && state.sent
                        };
                        if should_cancel {
                            let _ = session.engine.send(&json!({"type":"cancel"})).await;
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |message: acp::Dispatch, cx: ConnectionTo<Client>| {
                message.respond_with_error(acp::Error::method_not_found(), cx)
            },
            acp::on_receive_dispatch!(),
        )
        .connect_to(ByteStreams::new(write.compat_write(), incoming.compat()));
    let result = tokio::select! {result = serving => result.map_err(|error| std::io::Error::other(error.to_string())), _ = eof_rx => Ok(())};
    let sessions = std::mem::take(&mut *agent.sessions.lock().await);
    for session in sessions.into_values() {
        session.engine.stop().await;
    }
    result
}

impl Adapter {
    async fn new_session(&self, request: NewSessionRequest) -> acp::Result<NewSessionResponse> {
        if !request.cwd.is_absolute()
            || request
                .additional_directories
                .iter()
                .any(|path| !path.is_absolute())
        {
            return Err(acp::Error::invalid_params()
                .data("Gemini adapter requires absolute workspace directories"));
        }
        let mcp_servers = crate::mcp::translate(request.mcp_servers)?;
        let id = uuid::Uuid::new_v4().to_string();
        let auth = crate::auth::select(&self.environment)?;
        let init = json!({"type":"init", "sessionId":id, "cwd":request.cwd, "mcpServers":mcp_servers, "additionalDirectories":request.additional_directories, "auth":auth});
        let engine = Engine::start(
            &self.node,
            &self.module,
            &request.cwd,
            &self.environment,
            init,
        )
        .await
        .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        let catalog = crate::config::Catalog::from_engine(&engine.catalog)?;
        let options = catalog.options()?;
        self.sessions.lock().await.insert(
            id.clone(),
            Arc::new(Session {
                engine,
                turn: Arc::new(Mutex::new(())),
                prompt_start: Mutex::new(()),
                prompt_state: std::sync::Mutex::new(PromptState::default()),
                catalog: Mutex::new(catalog),
            }),
        );
        Ok(NewSessionResponse::new(id).config_options(options))
    }
    async fn set_config(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        if request.config_id.to_string() != "model" {
            return Err(acp::Error::invalid_params().data("Unknown configuration option"));
        }
        let session = self
            .sessions
            .lock()
            .await
            .get(&request.session_id.to_string())
            .cloned()
            .ok_or_else(acp::Error::invalid_params)?;
        let _turn = session.turn.try_lock().map_err(|_| {
            acp::Error::invalid_params().data("Cannot change the model during a prompt")
        })?;
        let value = serde_json::to_value(request).map_err(acp::Error::into_internal_error)?;
        let model = value["value"]
            .as_str()
            .ok_or_else(acp::Error::invalid_params)?;
        let mut catalog = session.catalog.lock().await;
        if !catalog.models.iter().any(|item| item == model) {
            return Err(acp::Error::invalid_params().data("Unknown Gemini model"));
        }
        session
            .engine
            .send(&json!({"type":"set_model", "model":model}))
            .await
            .map_err(acp::Error::into_internal_error)?;
        let response = session
            .engine
            .receive()
            .await
            .map_err(acp::Error::into_internal_error)?;
        if response["type"] != "configured" || response["model"] != model {
            return Err(acp::Error::internal_error().data("Gemini engine rejected model selection"));
        }
        catalog.current = model.to_owned();
        Ok(SetSessionConfigOptionResponse::new(catalog.options()?))
    }
}

struct EofRead<R> {
    inner: R,
    signal: Option<tokio::sync::oneshot::Sender<()>>,
}
impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for EofRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let has_space = buffer.remaining() > 0;
        let result = Pin::new(&mut self.inner).poll_read(cx, buffer);
        if matches!(result, Poll::Ready(Ok(())))
            && has_space
            && buffer.filled().len() == before
            && let Some(signal) = self.signal.take()
        {
            let _ = signal.send(());
        }
        result
    }
}
