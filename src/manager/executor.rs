//! Bounded manager job executor and cancellation bridge.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::manager::jobs::{JobJournal, JobKind, JobPhase};

/// The immutable context passed from a submitted job to its backend.
#[derive(Debug, Clone)]
pub struct ManagerExecutionRequest {
    pub id: String,
    pub kind: JobKind,
    pub payload_key: String,
    pub expected_generation: u64,
    pub runtime_lease: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagerExecutionOutcome {
    pub progress: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerExecutionError {
    Canceled,
    InputRejected(String),
    Domain(String),
    Unavailable,
}

pub trait ManagerExecutionBackend: Send + Sync + 'static {
    fn execute(
        &self,
        request: ManagerExecutionRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<ManagerExecutionOutcome, ManagerExecutionError>;
}

impl<T: ManagerExecutionBackend + ?Sized> ManagerExecutionBackend for Arc<T> {
    fn execute(
        &self,
        request: ManagerExecutionRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
        (**self).execute(request, cancel)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerExecutorError {
    QueueFull,
    QueueClosed,
    JobNotFound,
}

impl std::fmt::Display for ManagerExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::QueueFull => "manager executor queue is full",
            Self::QueueClosed => "manager executor queue is closed",
            Self::JobNotFound => "manager job not found",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ManagerExecutorError {}

struct QueuedExecution {
    request: ManagerExecutionRequest,
    cancel: Arc<AtomicBool>,
}

struct ExecutorState {
    sender: Mutex<Option<mpsc::Sender<QueuedExecution>>>,
    cancellation: Mutex<HashMap<String, Arc<AtomicBool>>>,
    journal: Arc<Mutex<JobJournal>>,
}

#[derive(Clone)]
pub struct ManagerExecutorHandle {
    state: Arc<ExecutorState>,
}

impl ManagerExecutorHandle {
    /// Enqueue a journaled job once. A second submission for the same ID is ignored.
    pub fn submit(&self, request: ManagerExecutionRequest) -> Result<(), ManagerExecutorError> {
        let mut journal = self.state.journal.lock().expect("manager journal poisoned");
        let phase = journal
            .get(&request.id)
            .map(|job| job.phase)
            .ok_or(ManagerExecutorError::JobNotFound)?;
        if !matches!(phase, JobPhase::Running | JobPhase::Cancelling) {
            return Ok(());
        }

        let mut cancellation = self
            .state
            .cancellation
            .lock()
            .expect("manager cancellation registry poisoned");
        if cancellation.contains_key(&request.id) {
            return Ok(());
        }
        let cancel = Arc::new(AtomicBool::new(phase == JobPhase::Cancelling));
        cancellation.insert(request.id.clone(), cancel.clone());

        let result = self
            .state
            .sender
            .lock()
            .expect("manager executor sender poisoned")
            .as_ref()
            .map_or(Err(ManagerExecutorError::QueueClosed), |sender| {
                sender
                    .try_send(QueuedExecution {
                        request: request.clone(),
                        cancel,
                    })
                    .map_err(|error| match error {
                        mpsc::error::TrySendError::Full(_) => ManagerExecutorError::QueueFull,
                        mpsc::error::TrySendError::Closed(_) => ManagerExecutorError::QueueClosed,
                    })
            });

        if let Err(error) = result {
            cancellation.remove(&request.id);
            let _ = journal.fail_if_running(&request.id, error.to_string());
        }
        result
    }

    /// Mark a running job as cancelling and notify its queued or active backend.
    pub fn cancel(&self, id: &str) -> Result<(), ManagerExecutorError> {
        // Journal first, then cancellation registry: the worker uses the same order.
        let mut journal = self.state.journal.lock().expect("manager journal poisoned");
        let phase = journal
            .get(id)
            .map(|job| job.phase)
            .ok_or(ManagerExecutorError::JobNotFound)?;
        if matches!(phase, JobPhase::Running | JobPhase::Cancelling) {
            journal
                .request_cancel(id)
                .map_err(|_| ManagerExecutorError::JobNotFound)?;
            if let Some(cancel) = self
                .state
                .cancellation
                .lock()
                .expect("manager cancellation registry poisoned")
                .get(id)
            {
                cancel.store(true, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    /// Close submissions and let the worker drain jobs already in the queue.
    pub fn shutdown_for_test(&self) {
        self.state
            .sender
            .lock()
            .expect("manager executor sender poisoned")
            .take();
    }
}

pub struct ManagerExecutor;

impl ManagerExecutor {
    pub fn start<B: ManagerExecutionBackend>(
        journal: Arc<Mutex<JobJournal>>,
        backend: B,
    ) -> (ManagerExecutorHandle, tokio::task::JoinHandle<()>) {
        let (sender, mut receiver) = mpsc::channel(16);
        let state = Arc::new(ExecutorState {
            sender: Mutex::new(Some(sender)),
            cancellation: Mutex::new(HashMap::new()),
            journal,
        });
        let handle = ManagerExecutorHandle {
            state: state.clone(),
        };
        let backend = Arc::new(backend);
        let guard = WorkerExitGuard(state.clone());
        let worker = tokio::spawn(async move {
            let _guard = guard;
            while let Some(queued) = receiver.recv().await {
                let id = queued.request.id.clone();
                let result = if queued.cancel.load(Ordering::SeqCst) {
                    Err(ManagerExecutionError::Canceled)
                } else {
                    let backend = backend.clone();
                    let cancel = queued.cancel.clone();
                    tokio::task::spawn_blocking(move || backend.execute(queued.request, cancel))
                        .await
                        .unwrap_or(Err(ManagerExecutionError::Unavailable))
                };
                let mut journal = state.journal.lock().expect("manager journal poisoned");
                if queued.cancel.load(Ordering::SeqCst)
                    || journal.is_cancelling(&id).unwrap_or(false)
                {
                    let _ = journal.fail_if_running(&id, "manager job canceled");
                } else {
                    match result {
                        Ok(outcome) => {
                            let _ = outcome.progress;
                            let _ = journal.succeed_if_running(&id);
                        }
                        Err(error) => {
                            let safe_message = public_error(&error);
                            tracing::warn!(job_id = %id, error = safe_message, "manager job failed");
                            let _ = journal.fail_if_running(&id, safe_message);
                        }
                    }
                }
                state
                    .cancellation
                    .lock()
                    .expect("manager cancellation registry poisoned")
                    .remove(&id);
            }
        });
        (handle, worker)
    }
}

/// An aborted worker must close every accepted job; no journal entry stays running.
struct WorkerExitGuard(Arc<ExecutorState>);

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        let mut journal = self.0.journal.lock().expect("manager journal poisoned");
        let mut cancellation = self
            .0
            .cancellation
            .lock()
            .expect("manager cancellation registry poisoned");
        for (id, cancel) in cancellation.drain() {
            cancel.store(true, Ordering::SeqCst);
            let _ = journal.fail_if_running(&id, "manager executor unavailable");
        }
    }
}

/// Backend text is never copied into a public job or a trace event. It may
/// contain URL userinfo, bearer tokens, query values, or raw build logs.
fn public_error(error: &ManagerExecutionError) -> &'static str {
    match error {
        ManagerExecutionError::Canceled => "manager job canceled",
        ManagerExecutionError::InputRejected(_) => "manager job input rejected",
        ManagerExecutionError::Domain(_) => "manager backend failed",
        ManagerExecutionError::Unavailable => "manager backend unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::jobs::{JobKind, JobPhase};

    #[derive(Clone)]
    enum FixtureOutcome {
        Success {
            progress: u8,
        },
        SuccessAfterCancel {
            started: Arc<AtomicBool>,
        },
        Error(String),
        InputRejected(String),
        Canceled,
        Panic,
        Block {
            started: Arc<AtomicBool>,
            release: Arc<AtomicBool>,
        },
    }

    struct FixtureBackend(FixtureOutcome);

    impl ManagerExecutionBackend for FixtureBackend {
        fn execute(
            &self,
            _request: ManagerExecutionRequest,
            cancel: Arc<AtomicBool>,
        ) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
            match &self.0 {
                FixtureOutcome::Success { progress } => Ok(ManagerExecutionOutcome {
                    progress: *progress,
                }),
                FixtureOutcome::SuccessAfterCancel { started } => {
                    started.store(true, Ordering::SeqCst);
                    while !cancel.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                    Ok(ManagerExecutionOutcome { progress: 100 })
                }
                FixtureOutcome::Error(message) => {
                    Err(ManagerExecutionError::Domain(message.clone()))
                }
                FixtureOutcome::InputRejected(message) => {
                    Err(ManagerExecutionError::InputRejected(message.clone()))
                }
                FixtureOutcome::Canceled => Err(ManagerExecutionError::Canceled),
                FixtureOutcome::Panic => panic!("backend panic secret"),
                FixtureOutcome::Block { started, release } => {
                    started.store(true, Ordering::SeqCst);
                    while !release.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                    Ok(ManagerExecutionOutcome { progress: 100 })
                }
            }
        }
    }

    fn test_executor(
        outcome: FixtureOutcome,
    ) -> (
        ManagerExecutorHandle,
        tokio::task::JoinHandle<()>,
        Arc<Mutex<JobJournal>>,
    ) {
        let journal = Arc::new(Mutex::new(JobJournal::new()));
        let (handle, worker) = ManagerExecutor::start(journal.clone(), FixtureBackend(outcome));
        (handle, worker, journal)
    }

    fn fixture_request(
        journal: &Arc<Mutex<JobJournal>>,
        kind: JobKind,
        key: &str,
    ) -> ManagerExecutionRequest {
        let id = journal.lock().unwrap().enqueue(kind, key).expect("enqueue");
        ManagerExecutionRequest {
            id,
            kind,
            payload_key: key.to_string(),
            expected_generation: 0,
            runtime_lease: None,
        }
    }

    #[tokio::test]
    async fn backend_success_reaches_succeeded() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Success { progress: 100 });
        let request = fixture_request(&journal, JobKind::Build, "fixture-build");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Succeeded
        );
    }

    #[tokio::test]
    async fn backend_error_reaches_failed_without_secret() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Error(
            "https://user:secret@example.invalid/?token=secret raw build log: secret".into(),
        ));
        let request = fixture_request(&journal, JobKind::Verify, "fixture-verify");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        let job = journal.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(job.phase, JobPhase::Failed);
        assert!(!job.error.contains("secret"));
        assert!(!job.error.contains("raw build log"));
    }

    #[tokio::test]
    async fn cancel_wins_over_late_backend_success() {
        let started = Arc::new(AtomicBool::new(false));
        let (handle, worker, journal) = test_executor(FixtureOutcome::SuccessAfterCancel {
            started: started.clone(),
        });
        let request = fixture_request(&journal, JobKind::Download, "fixture-download");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend started");
        handle.cancel(&id).expect("cancel");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        let job = journal.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(job.phase, JobPhase::Failed);
        assert!(job.cancel);
    }

    #[tokio::test]
    async fn closed_queue_marks_existing_job_failed() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Success { progress: 100 });
        handle.shutdown_for_test();
        let request = fixture_request(&journal, JobKind::Stage, "fixture-stage");
        let id = request.id.clone();
        let result = handle.submit(request);
        assert!(matches!(result, Err(ManagerExecutorError::QueueClosed)));
        worker.await.expect("worker");
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Failed
        );
    }

    #[tokio::test]
    async fn backend_input_rejection_is_redacted() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::InputRejected(
            "Bearer secret token=secret".into(),
        ));
        let request = fixture_request(&journal, JobKind::Fetch, "fixture-fetch");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        let job = journal.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(job.phase, JobPhase::Failed);
        assert!(!job.error.contains("secret"));
        assert!(!job.error.contains("Bearer"));
    }

    #[tokio::test]
    async fn backend_canceled_reaches_failed() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Canceled);
        let request = fixture_request(&journal, JobKind::Rollback, "fixture-rollback");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Failed
        );
    }

    #[tokio::test]
    async fn backend_panic_reaches_failed_without_panic_text() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Panic);
        let request = fixture_request(&journal, JobKind::Activate, "fixture-activate");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        let job = journal.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(job.phase, JobPhase::Failed);
        assert!(!job.error.contains("secret"));
    }

    #[tokio::test]
    async fn full_queue_marks_rejected_job_failed() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let (handle, worker, journal) = test_executor(FixtureOutcome::Block {
            started: started.clone(),
            release: release.clone(),
        });
        handle
            .submit(fixture_request(&journal, JobKind::Fetch, "active"))
            .expect("active queued");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend started");
        for index in 0..16 {
            let request = fixture_request(&journal, JobKind::Build, &format!("waiting-{index}"));
            handle.submit(request).expect("queue slot");
        }
        let rejected = fixture_request(&journal, JobKind::Verify, "rejected");
        let id = rejected.id.clone();
        assert_eq!(
            handle.submit(rejected),
            Err(ManagerExecutorError::QueueFull)
        );
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Failed
        );
        release.store(true, Ordering::SeqCst);
        handle.shutdown_for_test();
        worker.await.expect("worker");
    }

    #[tokio::test]
    async fn aborted_worker_fails_accepted_job() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let (handle, worker, journal) = test_executor(FixtureOutcome::Block {
            started: started.clone(),
            release: release.clone(),
        });
        let request = fixture_request(&journal, JobKind::Build, "active");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend started");
        worker.abort();
        assert!(worker.await.is_err());
        release.store(true, Ordering::SeqCst);
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Failed
        );
    }

    #[tokio::test]
    async fn worker_aborted_before_first_poll_fails_accepted_job() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Success { progress: 100 });
        let request = fixture_request(&journal, JobKind::Stage, "never-started");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        worker.abort();
        assert!(worker.await.is_err());
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Failed
        );
    }
}
