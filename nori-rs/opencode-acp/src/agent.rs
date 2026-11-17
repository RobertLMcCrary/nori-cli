//! ACP connection and session ownership for the experimental v2 adapter.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

use acp::Agent;
use acp::ByteStreams;
use acp::Client;
use acp::ConnectionTo;
use acp::schema::AgentCapabilities;
use acp::schema::CancelNotification;
use acp::schema::CloseSessionRequest;
use acp::schema::DeleteSessionRequest;
use acp::schema::ForkSessionRequest;
use acp::schema::InitializeRequest;
use acp::schema::InitializeResponse;
use acp::schema::ListSessionsRequest;
use acp::schema::LoadSessionRequest;
use acp::schema::McpCapabilities;
use acp::schema::NewSessionRequest;
use acp::schema::NewSessionResponse;
use acp::schema::PromptCapabilities;
use acp::schema::PromptRequest;
use acp::schema::ResumeSessionRequest;
use acp::schema::SessionAdditionalDirectoriesCapabilities;
use acp::schema::SessionCapabilities;
use acp::schema::SessionCloseCapabilities;
use acp::schema::SessionDeleteCapabilities;
use acp::schema::SessionForkCapabilities;
use acp::schema::SessionListCapabilities;
use acp::schema::SessionResumeCapabilities;
use acp::schema::SetSessionConfigOptionRequest;
use acp::schema::SetSessionConfigOptionResponse;
use acp::schema::SetSessionModeRequest;
use acp::schema::SetSessionModeResponse;
use agent_client_protocol as acp;
use reqwest::Method;
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::OpenCodeServer;

pub(crate) struct Adapter {
    pub(crate) executable: PathBuf,
    pub(crate) environment: HashMap<OsString, OsString>,
    pub(crate) sessions: Mutex<HashMap<String, Arc<Session>>>,
    pub(crate) known_children: Arc<Mutex<HashMap<String, Arc<Session>>>>,
}

pub(crate) struct Session {
    pub(crate) server: Arc<OpenCodeServer>,
    pub(crate) turn: Arc<Mutex<()>>,
    pub(crate) cancelled: AtomicBool,
    pub(crate) closed: AtomicBool,
    pub(crate) native_child: bool,
    pub(crate) cancel_notify: tokio::sync::Notify,
    pub(crate) permission_reply: Mutex<()>,
    pub(crate) catalog: Mutex<crate::config::Catalog>,
}

/// Serve the experimental ACP agent over an in-memory pipe.
///
/// This is opt-in while OpenCode ACP conformance is being completed.
pub async fn serve_over_pipe(
    pipe: tokio::io::DuplexStream,
    executable: PathBuf,
    environment: HashMap<OsString, OsString>,
) -> std::io::Result<()> {
    let agent = Arc::new(Adapter {
        executable,
        environment,
        sessions: Mutex::new(HashMap::new()),
        known_children: Arc::new(Mutex::new(HashMap::new())),
    });
    let (read, write) = tokio::io::split(pipe);
    let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
    let incoming = EofRead {
        inner: read,
        signal: Some(eof_tx),
    };
    let serving = Agent.builder()
        .name("nori-opencode-acp")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                let sessions = SessionCapabilities::new().list(SessionListCapabilities::new()).delete(SessionDeleteCapabilities::new()).additional_directories(SessionAdditionalDirectoriesCapabilities::new()).fork(SessionForkCapabilities::new()).resume(SessionResumeCapabilities::new()).close(SessionCloseCapabilities::new());
                responder.respond(InitializeResponse::new(request.protocol_version).agent_capabilities(AgentCapabilities::new().load_session(true).session_capabilities(sessions).mcp_capabilities(McpCapabilities::new().http(true)).prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))))
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: SetSessionConfigOptionRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    cx.spawn(async move { responder.respond_with_result(agent.set_config(request).await) })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: SetSessionModeRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    let client = cx.clone();
                    cx.spawn(async move {
                        let id = request.session_id.to_string();
                        let result = agent.set_mode(request).await;
                        match result {
                            Ok(options) => {
                                responder.respond(SetSessionModeResponse::new())?;
                                client.send_notification(acp::UntypedMessage::new("session/update", json!({"sessionId":id,"update":{"sessionUpdate":"config_option_update","configOptions":options}}))?)?;
                                Ok(())
                            }
                            Err(error) => responder.respond_with_result(Err(error)),
                        }
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: LoadSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    let client = cx.clone();
                    cx.spawn(async move {
                        let id = request.session_id.to_string();
                        let result = agent.load_session(request, client.clone()).await;
                        let succeeded = result.is_ok();
                        responder.respond_with_result(result)?;
                        if succeeded { agent.send_commands(&id, &client).await?; }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: NewSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    let client = cx.clone();
                    cx.spawn(async move {
                        let result = agent.new_session(request).await;
                        let id = result.as_ref().ok().map(|response| response.session_id.to_string());
                        responder.respond_with_result(result)?;
                        if let Some(id) = id { agent.send_commands(&id, &client).await?; }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: ResumeSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    let client = cx.clone();
                    cx.spawn(async move {
                        let id = request.session_id.to_string();
                        let result = agent.resume_session(request).await;
                        let succeeded = result.is_ok();
                        responder.respond_with_result(result)?;
                        if succeeded { agent.send_commands(&id, &client).await?; }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: ForkSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    let client = cx.clone();
                    cx.spawn(async move {
                        let result = agent.fork_session(request).await;
                        let id = result.as_ref().ok().map(|response| response.session_id.to_string());
                        responder.respond_with_result(result)?;
                        if let Some(id) = id { agent.send_commands(&id, &client).await?; }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: ListSessionsRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    cx.spawn(async move { responder.respond_with_result(agent.list_sessions(request).await) })?;
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
                    cx.spawn(async move { responder.respond_with_result(agent.close_session(request).await) })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: DeleteSessionRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    cx.spawn(async move { responder.respond_with_result(agent.delete_session(request).await) })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let agent = agent.clone();
                async move |request: CancelNotification, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    cx.spawn(async move { agent.cancel(request).await; Ok(()) })?;
                    Ok(())
                }
            },
            acp::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let agent = agent.clone();
                async move |request: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let agent = agent.clone();
                    let client = cx.clone();
                    let prepared = agent.reserve_prompt(&request).await;
                    cx.spawn(async move {
                        let result = match prepared {
                            Ok((session, turn)) => crate::turn::run(session, request, client, agent.known_children.clone(), turn).await,
                            Err(error) => Err(error),
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_dispatch(
            async move |message: acp::Dispatch, cx: ConnectionTo<Client>| {
                match message {
                    acp::Dispatch::Response(result, router) => router.respond_with_result(result),
                    other => other.respond_with_error(acp::Error::method_not_found(), cx),
                }
            },
            acp::on_receive_dispatch!(),
        )
        .connect_to(ByteStreams::new(write.compat_write(), incoming.compat()));
    let result = tokio::select! {
        result = serving => result.map_err(|error| std::io::Error::other(error.to_string())),
        _ = eof_rx => Ok(()),
    };
    for session in agent.sessions.lock().await.values() {
        session.closed.store(true, Ordering::SeqCst);
    }
    result
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

impl Adapter {
    async fn send_commands(&self, id: &str, client: &ConnectionTo<Client>) -> acp::Result<()> {
        let session = self
            .sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(acp::Error::invalid_params)?;
        let commands = session.catalog.lock().await.commands().to_vec();
        client.send_notification(acp::UntypedMessage::new("session/update", json!({"sessionId":id,"update":{"sessionUpdate":"available_commands_update","availableCommands":commands}}))?)
    }

    async fn new_session(&self, request: NewSessionRequest) -> acp::Result<NewSessionResponse> {
        let grant = crate::directories::update(
            &request.cwd,
            &serde_json::Value::Null,
            request.additional_directories,
        )?;
        let server = OpenCodeServer::start(&self.executable, &request.cwd, &self.environment)
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        let result = server
            .request(Method::POST, &["session"], Some(&grant))
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        crate::mcp::register(&server, request.mcp_servers).await?;
        let session_id = result["data"]["id"]
            .as_str()
            .ok_or_else(|| {
                acp::Error::internal_error().data("OpenCode returned a session without an id")
            })?
            .to_owned();
        let catalog = crate::config::Catalog::load(&server, &result["data"]).await?;
        let options = catalog.options()?;
        self.sessions.lock().await.insert(
            session_id.clone(),
            Arc::new(Session::new(Arc::new(server), catalog)),
        );
        Ok(NewSessionResponse::new(session_id).config_options(options))
    }

    async fn cancel(&self, request: CancelNotification) {
        let id = request.session_id.to_string();
        let server = self.resolve_session(&id).await.ok();
        if let Some(session) = server {
            if !session.native_child && session.turn.try_lock().is_ok() {
                return;
            }
            let _permission_reply = session.permission_reply.lock().await;
            session.cancelled.store(true, Ordering::SeqCst);
            session.cancel_notify.notify_one();
            let _ = session
                .server
                .request(
                    Method::POST,
                    &["session", &id, "interrupt"],
                    Some(&json!({})),
                )
                .await;
        }
    }

    async fn reserve_prompt(
        &self,
        request: &PromptRequest,
    ) -> acp::Result<(Arc<Session>, tokio::sync::OwnedMutexGuard<()>)> {
        let id = request.session_id.to_string();
        let session = self.resolve_session(&id).await?;
        let turn = session.turn.clone().try_lock_owned().map_err(|_| {
            acp::Error::invalid_params().data("A prompt is already running in this session")
        })?;
        session.cancelled.store(false, Ordering::SeqCst);
        use futures::FutureExt;
        let _ = session.cancel_notify.notified().now_or_never();
        Ok((session, turn))
    }

    async fn set_config(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        let id = request.session_id.to_string();
        let session = self.resolve_session(&id).await?;
        let value = serde_json::to_value(&request).map_err(acp::Error::into_internal_error)?;
        let value = value["value"]
            .as_str()
            .ok_or_else(acp::Error::invalid_params)?;
        let options = session
            .catalog
            .lock()
            .await
            .set(&session.server, &id, &request.config_id.to_string(), value)
            .await?;
        Ok(SetSessionConfigOptionResponse::new(options))
    }

    async fn set_mode(
        &self,
        request: SetSessionModeRequest,
    ) -> acp::Result<Vec<acp::schema::SessionConfigOption>> {
        let id = request.session_id.to_string();
        let session = self.resolve_session(&id).await?;
        let options = session
            .catalog
            .lock()
            .await
            .set(&session.server, &id, "mode", &request.mode_id.to_string())
            .await?;
        Ok(options)
    }
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
