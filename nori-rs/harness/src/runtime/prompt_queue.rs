//! Serialize prompt admission without blocking the session command actor.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::backend::AcpBackend;
use crate::backend::PromptAdmission;
use nori_protocol::acp;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;

struct PromptJob {
    id: i64,
    content: Vec<acp::v1::ContentBlock>,
    meta: Option<acp::v1::Meta>,
    admission: PromptAdmission,
    response_tx: oneshot::Sender<anyhow::Result<acp::v1::RequestId>>,
    cancel_tx: watch::Sender<bool>,
    cancel_rx: watch::Receiver<bool>,
}

#[derive(Default)]
struct PromptControl {
    active: Option<(i64, watch::Sender<bool>)>,
    queued: VecDeque<(i64, watch::Sender<bool>)>,
}

/// FIFO prompt admission worker owned by the Harness command actor.
pub(super) struct PromptQueue {
    tx: mpsc::UnboundedSender<PromptJob>,
    control: Arc<Mutex<PromptControl>>,
    shutdown_tx: watch::Sender<bool>,
    worker: Option<JoinHandle<()>>,
    next_id: i64,
}

impl PromptQueue {
    pub(super) fn start(backend: Arc<AcpBackend>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let control = Arc::new(Mutex::new(PromptControl::default()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = tokio::spawn(run_prompt_worker(
            backend,
            rx,
            Arc::clone(&control),
            shutdown_rx,
        ));
        Self {
            tx,
            control,
            shutdown_tx,
            worker: Some(worker),
            next_id: 0,
        }
    }

    pub(super) async fn enqueue(
        &mut self,
        content: Vec<acp::v1::ContentBlock>,
        meta: Option<acp::v1::Meta>,
        admission: PromptAdmission,
        response_tx: oneshot::Sender<anyhow::Result<acp::v1::RequestId>>,
    ) {
        self.next_id += 1;
        let id = self.next_id;
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.control
            .lock()
            .await
            .queued
            .push_back((id, cancel_tx.clone()));
        let job = PromptJob {
            id,
            content,
            meta,
            admission,
            response_tx,
            cancel_tx,
            cancel_rx,
        };
        if let Err(error) = self.tx.send(job) {
            let job = error.0;
            self.control
                .lock()
                .await
                .queued
                .retain(|(queued_id, _)| *queued_id != id);
            let _ = job.response_tx.send(Err(anyhow::anyhow!(
                "ACP prompt admission worker is closed"
            )));
        }
    }

    /// Cancel the prompt currently being admitted, or the oldest queued prompt.
    pub(super) async fn cancel_pending(&self) {
        let control = self.control.lock().await;
        if let Some((_, cancel)) = &control.active {
            cancel.send_replace(true);
        } else if let Some((_, cancel)) = control.queued.front() {
            cancel.send_replace(true);
        }
    }

    /// The backend phase does not include prompts still waiting for policy admission.
    pub(super) async fn is_busy(&self) -> bool {
        let control = self.control.lock().await;
        control.active.is_some() || !control.queued.is_empty()
    }

    /// Stop prompt admission before the backend is shut down.
    pub(super) async fn shutdown(&mut self) {
        self.shutdown_tx.send_replace(true);
        {
            let control = self.control.lock().await;
            if let Some((_, cancel)) = &control.active {
                cancel.send_replace(true);
            }
            for (_, cancel) in &control.queued {
                cancel.send_replace(true);
            }
        }
        if let Some(worker) = self.worker.take()
            && let Err(error) = worker.await
        {
            tracing::warn!(%error, "prompt admission worker stopped unexpectedly");
        }
    }
}

async fn run_prompt_worker(
    backend: Arc<AcpBackend>,
    mut rx: mpsc::UnboundedReceiver<PromptJob>,
    control: Arc<Mutex<PromptControl>>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let job = tokio::select! {
            biased;
            _ = wait_until_true(&mut shutdown) => break,
            job = rx.recv() => match job {
                Some(job) => job,
                None => break,
            },
        };
        {
            let mut control = control.lock().await;
            control.queued.retain(|(queued_id, _)| *queued_id != job.id);
            control.active = Some((job.id, job.cancel_tx.clone()));
        }
        let job_id = job.id;
        backend
            .submit_prompt_cancellable(
                job.content,
                job.meta,
                job.admission,
                job.cancel_rx,
                job.response_tx,
            )
            .await;
        let mut control = control.lock().await;
        if control
            .active
            .as_ref()
            .is_some_and(|(active_id, _)| *active_id == job_id)
        {
            control.active = None;
        }
        if *shutdown.borrow_and_update() {
            break;
        }
    }
}

async fn wait_until_true(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            return;
        }
    }
}
