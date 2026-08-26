use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionState {
    Serving,
    Draining,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionSnapshot {
    pub state: AdmissionState,
    pub in_flight: usize,
    pub max_in_flight: usize,
    pub drain_generation: Option<u64>,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionError {
    #[error("admission is not serving")]
    NotServing,
    #[error("target upstream is not ready")]
    UpstreamNotReady,
    #[error("admission is at capacity")]
    AtCapacity,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum DrainError {
    #[error("drain timed out")]
    Timeout,
    #[error("drain generation changed")]
    GenerationChanged,
}

#[derive(Debug)]
struct Inner {
    state: AdmissionState,
    in_flight: usize,
    max_in_flight: usize,
    drain_generation: Option<u64>,
}

#[derive(Debug)]
struct Shared {
    inner: Mutex<Inner>,
    zero_in_flight: Notify,
}

#[derive(Debug, Clone)]
pub struct AdmissionGate {
    shared: Arc<Shared>,
}

#[derive(Debug)]
pub struct AdmissionPermit {
    shared: Arc<Shared>,
}

impl AdmissionGate {
    pub fn new(max_in_flight: usize) -> Self {
        assert!(max_in_flight > 0, "max_in_flight must be greater than zero");
        Self {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    state: AdmissionState::Blocked,
                    in_flight: 0,
                    max_in_flight,
                    drain_generation: None,
                }),
                zero_in_flight: Notify::new(),
            }),
        }
    }

    pub fn snapshot(&self) -> AdmissionSnapshot {
        let inner = self.lock();
        AdmissionSnapshot {
            state: inner.state,
            in_flight: inner.in_flight,
            max_in_flight: inner.max_in_flight,
            drain_generation: inner.drain_generation,
        }
    }

    pub fn start_serving(&self) {
        let mut inner = self.lock();
        inner.state = AdmissionState::Serving;
        inner.drain_generation = None;
    }

    pub fn block(&self) {
        self.lock().state = AdmissionState::Blocked;
    }

    /// Keeps admission blocked while retiring the generation marker from a completed drain.
    ///
    /// Recovery transitions intentionally remain blocked after reaching a stable state. Unlike
    /// `start_serving`, that state must not clear admission by itself, but the next transition
    /// still needs to establish a fresh drain generation.
    pub fn reset_blocked_generation(&self) {
        let mut inner = self.lock();
        debug_assert_eq!(inner.state, AdmissionState::Blocked);
        debug_assert_eq!(inner.in_flight, 0);
        inner.drain_generation = None;
    }

    pub fn try_acquire(&self, upstream_ready: bool) -> Result<AdmissionPermit, AdmissionError> {
        let mut inner = self.lock();
        if inner.state != AdmissionState::Serving {
            return Err(AdmissionError::NotServing);
        }
        self.acquire_permit(&mut inner, upstream_ready)
    }

    /// Acquire the one internal recovery-canary slot without changing public admission state.
    /// The caller must authenticate and consume the recovery permit before calling this method.
    pub fn try_acquire_recovery(
        &self,
        upstream_ready: bool,
    ) -> Result<AdmissionPermit, AdmissionError> {
        let mut inner = self.lock();
        self.acquire_permit(&mut inner, upstream_ready)
    }

    fn acquire_permit(
        &self,
        inner: &mut Inner,
        upstream_ready: bool,
    ) -> Result<AdmissionPermit, AdmissionError> {
        if !upstream_ready {
            return Err(AdmissionError::UpstreamNotReady);
        }
        if inner.in_flight >= inner.max_in_flight {
            return Err(AdmissionError::AtCapacity);
        }
        inner.in_flight += 1;
        Ok(AdmissionPermit {
            shared: self.shared.clone(),
        })
    }

    pub fn begin_drain(&self, generation: u64) -> Result<(), DrainError> {
        let mut inner = self.lock();
        if let Some(active) = inner.drain_generation
            && active != generation
        {
            return Err(DrainError::GenerationChanged);
        }
        inner.state = AdmissionState::Draining;
        inner.drain_generation = Some(generation);
        Ok(())
    }

    pub fn accepts_drained_ack(&self, generation: u64) -> bool {
        let inner = self.lock();
        inner.state == AdmissionState::Draining && inner.drain_generation == Some(generation)
    }

    pub async fn wait_for_zero(&self, timeout: Duration) -> Result<(), DrainError> {
        tokio::time::timeout(timeout, async {
            loop {
                let notified = self.shared.zero_in_flight.notified();
                if self.lock().in_flight == 0 {
                    return;
                }
                notified.await;
            }
        })
        .await
        .map_err(|_| DrainError::Timeout)
    }

    pub async fn drain(&self, generation: u64, timeout: Duration) -> Result<(), DrainError> {
        self.begin_drain(generation)?;
        self.wait_for_zero(timeout).await?;
        let mut inner = self.lock();
        if inner.drain_generation != Some(generation) {
            return Err(DrainError::GenerationChanged);
        }
        inner.state = AdmissionState::Blocked;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let became_zero = {
            let mut inner = self
                .shared
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            debug_assert!(inner.in_flight > 0);
            inner.in_flight -= 1;
            inner.in_flight == 0
        };
        if became_zero {
            self.shared.zero_in_flight.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;

    #[test]
    fn serving_requires_ready_target_and_capacity() {
        let gate = AdmissionGate::new(1);
        assert_eq!(
            gate.try_acquire(true).unwrap_err(),
            AdmissionError::NotServing
        );
        gate.start_serving();
        assert_eq!(
            gate.try_acquire(false).unwrap_err(),
            AdmissionError::UpstreamNotReady
        );
        let permit = gate.try_acquire(true).unwrap();
        assert_eq!(
            gate.try_acquire(true).unwrap_err(),
            AdmissionError::AtCapacity
        );
        drop(permit);
        assert_eq!(gate.snapshot().in_flight, 0);
    }

    #[tokio::test]
    async fn drain_serializes_with_new_permits_and_blocks_at_zero() {
        let gate = AdmissionGate::new(2);
        gate.start_serving();
        let permit = gate.try_acquire(true).unwrap();
        gate.begin_drain(7).unwrap();
        assert_eq!(
            gate.try_acquire(true).unwrap_err(),
            AdmissionError::NotServing
        );
        drop(permit);
        gate.drain(7, Duration::from_secs(1)).await.unwrap();
        assert_eq!(gate.snapshot().state, AdmissionState::Blocked);
        assert_eq!(gate.snapshot().in_flight, 0);
    }

    #[tokio::test]
    async fn drain_timeout_does_not_drop_an_active_permit() {
        let gate = AdmissionGate::new(1);
        gate.start_serving();
        let permit = gate.try_acquire(true).unwrap();
        assert_eq!(
            gate.drain(9, Duration::from_millis(10)).await.unwrap_err(),
            DrainError::Timeout
        );
        assert_eq!(gate.snapshot().in_flight, 1);
        assert_eq!(gate.snapshot().state, AdmissionState::Draining);
        drop(permit);
        gate.drain(9, Duration::from_secs(1)).await.unwrap();
    }

    #[test]
    fn recovery_permit_is_one_request_exception_while_blocked() {
        let gate = AdmissionGate::new(1);
        let permit = gate.try_acquire_recovery(true).unwrap();
        assert_eq!(gate.snapshot().in_flight, 1);
        assert_eq!(
            gate.try_acquire_recovery(true).unwrap_err(),
            AdmissionError::AtCapacity
        );
        drop(permit);
        assert_eq!(gate.snapshot().state, AdmissionState::Blocked);
        assert_eq!(gate.snapshot().in_flight, 0);
    }

    #[tokio::test]
    async fn reset_blocked_generation_keeps_admission_blocked() {
        let gate = AdmissionGate::new(1);
        gate.start_serving();
        gate.drain(9, Duration::from_secs(1)).await.unwrap();
        assert_eq!(gate.snapshot().drain_generation, Some(9));

        gate.reset_blocked_generation();

        assert_eq!(gate.snapshot().state, AdmissionState::Blocked);
        assert_eq!(gate.snapshot().drain_generation, None);
        assert_eq!(
            gate.try_acquire(true).unwrap_err(),
            AdmissionError::NotServing
        );
    }

    #[test]
    fn rejects_ack_and_drain_from_another_generation() {
        let gate = AdmissionGate::new(1);
        gate.start_serving();
        gate.begin_drain(11).unwrap();
        assert!(gate.accepts_drained_ack(11));
        assert!(!gate.accepts_drained_ack(10));
        assert_eq!(
            gate.begin_drain(12).unwrap_err(),
            DrainError::GenerationChanged
        );
    }

    #[tokio::test]
    async fn cancellation_storm_returns_in_flight_to_zero() {
        const REQUESTS: usize = 128;
        let gate = AdmissionGate::new(REQUESTS);
        gate.start_serving();
        let mut tasks = Vec::new();
        for _ in 0..REQUESTS {
            let gate = gate.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = gate.try_acquire(true).unwrap();
                pending::<()>().await;
            }));
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while gate.snapshot().in_flight != REQUESTS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
        gate.wait_for_zero(Duration::from_secs(1)).await.unwrap();
        assert_eq!(gate.snapshot().in_flight, 0);
    }
}
