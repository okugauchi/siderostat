use super::{ChildIdentity, PersistentClusterState, ProcessControlError, ProcessController};
use std::{io, net::SocketAddr, time::Duration};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartDecision {
    StartSolo {
        baseline_generation: u64,
    },
    ManualIntervention {
        baseline_generation: u64,
        reason: RestartManualReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartManualReason {
    PersistedChildIdentityMismatch,
    PersistedChildStopFailed,
    RequiredPortOccupied,
    RequiredAddressUnavailable,
}

#[derive(Debug, Error)]
pub enum RestartReconcileError {
    #[error("invalid persisted child hash")]
    InvalidChildHash,
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub async fn reconcile_restart<F>(
    state: Option<&PersistentClusterState>,
    controller: &ProcessController,
    stop_timeout: Duration,
    allow_sigkill: bool,
    mut port_available: F,
) -> Result<RestartDecision, RestartReconcileError>
where
    F: FnMut() -> io::Result<bool>,
{
    let baseline_generation = state.map_or(0, |state| state.generation);
    if let Some((state, child)) =
        state.and_then(|state| state.child.as_ref().map(|child| (state, child)))
    {
        let Some(profile_id) = state.active_profile.clone() else {
            return Ok(RestartDecision::ManualIntervention {
                baseline_generation,
                reason: RestartManualReason::PersistedChildIdentityMismatch,
            });
        };
        let identity = ChildIdentity {
            pid: child.pid,
            executable: child.executable.clone(),
            argv_sha256: decode_hash(&child.argv_sha256)?,
            profile_id,
            generation: state.generation,
            spawned_at_millis: child.spawned_at_millis,
            process_start_micros: child.process_start_micros,
        };
        match controller
            .stop_recovered_owned(
                &identity,
                stop_timeout,
                Duration::from_millis(20),
                allow_sigkill,
            )
            .await
        {
            Ok(()) | Err(ProcessControlError::NotRunning) => {}
            Err(ProcessControlError::IdentityMismatch) => {
                return Ok(RestartDecision::ManualIntervention {
                    baseline_generation,
                    reason: RestartManualReason::PersistedChildIdentityMismatch,
                });
            }
            Err(_) => {
                return Ok(RestartDecision::ManualIntervention {
                    baseline_generation,
                    reason: RestartManualReason::PersistedChildStopFailed,
                });
            }
        }
    }
    match port_available() {
        Ok(true) => Ok(RestartDecision::StartSolo {
            baseline_generation,
        }),
        Ok(false) => Ok(RestartDecision::ManualIntervention {
            baseline_generation,
            reason: RestartManualReason::RequiredPortOccupied,
        }),
        Err(_) => Ok(RestartDecision::ManualIntervention {
            baseline_generation,
            reason: RestartManualReason::RequiredAddressUnavailable,
        }),
    }
}

pub fn required_port_available(address: SocketAddr) -> io::Result<bool> {
    match std::net::TcpListener::bind(address) {
        Ok(listener) => {
            drop(listener);
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => Ok(false),
        Err(error) => Err(error),
    }
}

fn decode_hash(value: &str) -> Result<[u8; 32], RestartReconcileError> {
    if value.len() != 64 {
        return Err(RestartReconcileError::InvalidChildHash);
    }
    let mut hash = [0_u8; 32];
    for (index, output) in hash.iter_mut().enumerate() {
        *output = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| RestartReconcileError::InvalidChildHash)?;
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{
        ObservedProcess, PERSISTENT_STATE_SCHEMA_VERSION, PersistentChild, PersistentMode,
        PersistentProxyTarget, ProcessInspector, ProcessSignal, ProcessSignaler, argv_sha256,
    };
    use std::{
        ffi::OsString,
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct Inspector(Arc<Mutex<Option<ObservedProcess>>>);

    impl ProcessInspector for Inspector {
        fn observe(&self, _pid: u32) -> io::Result<Option<ObservedProcess>> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    struct Signaler {
        observed: Arc<Mutex<Option<ObservedProcess>>>,
        signals: Arc<Mutex<Vec<ProcessSignal>>>,
    }

    impl ProcessSignaler for Signaler {
        fn signal_process_group(&self, _pid: u32, signal: ProcessSignal) -> io::Result<()> {
            self.signals.lock().unwrap().push(signal);
            *self.observed.lock().unwrap() = None;
            Ok(())
        }
    }

    fn fixture() -> (PersistentClusterState, ObservedProcess) {
        let observed = ObservedProcess {
            pid: 42,
            executable: PathBuf::from("/opt/ds4/ds4-server"),
            argv: vec![OsString::from("-m"), OsString::from("/model.gguf")],
            start_time_micros: 200,
        };
        let state = PersistentClusterState {
            schema_version: PERSISTENT_STATE_SCHEMA_VERSION,
            generation: 9,
            desired_mode: PersistentMode::SoloStandalone,
            last_stable_mode: PersistentMode::SoloStandalone,
            cluster_state: "solo-standalone-ready".into(),
            proxy_target: PersistentProxyTarget::LocalStandalone,
            active_profile: Some("standalone".into()),
            child: Some(PersistentChild {
                pid: observed.pid,
                executable: observed.executable.clone(),
                argv_sha256: argv_sha256(observed.executable.as_os_str(), &observed.argv)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
                spawned_at_millis: 100,
                process_start_micros: observed.start_time_micros,
            }),
            last_failure: None,
        };
        (state, observed)
    }

    #[tokio::test]
    async fn matching_owned_child_is_stopped_before_start_convergence() {
        let (state, observed) = fixture();
        let slot = Arc::new(Mutex::new(Some(observed)));
        let signals = Arc::new(Mutex::new(Vec::new()));
        let controller = ProcessController::new(
            Arc::new(Inspector(slot.clone())),
            Arc::new(Signaler {
                observed: slot,
                signals: signals.clone(),
            }),
        );
        assert_eq!(
            reconcile_restart(
                Some(&state),
                &controller,
                Duration::from_millis(100),
                false,
                || Ok(true),
            )
            .await
            .unwrap(),
            RestartDecision::StartSolo {
                baseline_generation: 9
            }
        );
        assert_eq!(
            signals.lock().unwrap().as_slice(),
            &[ProcessSignal::Terminate]
        );
    }

    #[tokio::test]
    async fn mismatch_or_unknown_port_owner_is_manual_and_never_signaled() {
        let (state, mut observed) = fixture();
        observed.start_time_micros += 1;
        let slot = Arc::new(Mutex::new(Some(observed)));
        let signals = Arc::new(Mutex::new(Vec::new()));
        let controller = ProcessController::new(
            Arc::new(Inspector(slot.clone())),
            Arc::new(Signaler {
                observed: slot,
                signals: signals.clone(),
            }),
        );
        assert_eq!(
            reconcile_restart(
                Some(&state),
                &controller,
                Duration::from_millis(100),
                false,
                || Ok(true),
            )
            .await
            .unwrap(),
            RestartDecision::ManualIntervention {
                baseline_generation: 9,
                reason: RestartManualReason::PersistedChildIdentityMismatch,
            }
        );
        assert!(signals.lock().unwrap().is_empty());

        let empty_controller = ProcessController::new(
            Arc::new(Inspector(Arc::new(Mutex::new(None)))),
            Arc::new(Signaler {
                observed: Arc::new(Mutex::new(None)),
                signals: signals.clone(),
            }),
        );
        assert_eq!(
            reconcile_restart(
                None,
                &empty_controller,
                Duration::from_millis(100),
                false,
                || Ok(false)
            )
            .await
            .unwrap(),
            RestartDecision::ManualIntervention {
                baseline_generation: 0,
                reason: RestartManualReason::RequiredPortOccupied,
            }
        );
        assert!(signals.lock().unwrap().is_empty());
    }
}
