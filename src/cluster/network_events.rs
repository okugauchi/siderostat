use std::{sync::Arc, time::Duration};
use thiserror::Error;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkEventKind {
    InterfaceList,
    Link,
    Ipv4,
    Setup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkEvent {
    pub generation: u64,
    pub kind: NetworkEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanReason {
    Initial,
    DebouncedNotification,
    Reconcile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RescanRequest {
    pub generation: u64,
    pub reason: RescanReason,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SpawnNetworkMonitorError {
    #[error("network event channel capacity must be greater than zero")]
    ZeroEventCapacity,
    #[error("network debounce and reconcile intervals must be greater than zero")]
    ZeroInterval,
}

struct Control {
    events: mpsc::Sender<NetworkEvent>,
    cancelled: watch::Sender<bool>,
}

impl Drop for Control {
    fn drop(&mut self) {
        self.cancelled.send_replace(true);
    }
}

#[derive(Clone)]
pub struct NetworkEventHandle {
    control: Arc<Control>,
    generation: u64,
}

impl NetworkEventHandle {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn try_notify(
        &self,
        kind: NetworkEventKind,
    ) -> Result<(), mpsc::error::TrySendError<NetworkEvent>> {
        self.control.events.try_send(NetworkEvent {
            generation: self.generation,
            kind,
        })
    }

    #[cfg(test)]
    fn try_notify_with_generation(
        &self,
        generation: u64,
        kind: NetworkEventKind,
    ) -> Result<(), mpsc::error::TrySendError<NetworkEvent>> {
        self.control
            .events
            .try_send(NetworkEvent { generation, kind })
    }
}

pub fn spawn_network_event_monitor(
    generation: u64,
    debounce: Duration,
    reconcile_interval: Duration,
    event_capacity: usize,
    rescans: mpsc::Sender<RescanRequest>,
) -> Result<(NetworkEventHandle, tokio::task::JoinHandle<()>), SpawnNetworkMonitorError> {
    if event_capacity == 0 {
        return Err(SpawnNetworkMonitorError::ZeroEventCapacity);
    }
    if debounce.is_zero() || reconcile_interval.is_zero() {
        return Err(SpawnNetworkMonitorError::ZeroInterval);
    }
    let (events, mut event_receiver) = mpsc::channel(event_capacity);
    let (cancelled, mut cancellation) = watch::channel(false);
    let handle = NetworkEventHandle {
        control: Arc::new(Control { events, cancelled }),
        generation,
    };
    let task = tokio::spawn(async move {
        if rescans
            .send(RescanRequest {
                generation,
                reason: RescanReason::Initial,
            })
            .await
            .is_err()
        {
            return;
        }
        let mut reconcile = tokio::time::interval(reconcile_interval);
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        reconcile.tick().await;
        let mut deadline = None;
        loop {
            if let Some(at) = deadline {
                tokio::select! {
                    biased;
                    changed = cancellation.changed() => {
                        if changed.is_err() || *cancellation.borrow() { return; }
                    }
                    event = event_receiver.recv() => match event {
                        Some(event) if event.generation == generation => {
                            deadline = Some(tokio::time::Instant::now() + debounce);
                        }
                        Some(_) => {}
                        None => return,
                    },
                    _ = tokio::time::sleep_until(at) => {
                        deadline = None;
                        if rescans.send(RescanRequest {
                            generation,
                            reason: RescanReason::DebouncedNotification,
                        }).await.is_err() { return; }
                    }
                    _ = reconcile.tick() => {
                        if rescans.send(RescanRequest {
                            generation,
                            reason: RescanReason::Reconcile,
                        }).await.is_err() { return; }
                    }
                }
            } else {
                tokio::select! {
                    biased;
                    changed = cancellation.changed() => {
                        if changed.is_err() || *cancellation.borrow() { return; }
                    }
                    event = event_receiver.recv() => match event {
                        Some(event) if event.generation == generation => {
                            deadline = Some(tokio::time::Instant::now() + debounce);
                        }
                        Some(_) => {}
                        None => return,
                    },
                    _ = reconcile.tick() => {
                        if rescans.send(RescanRequest {
                            generation,
                            reason: RescanReason::Reconcile,
                        }).await.is_err() { return; }
                    }
                }
            }
        }
    });
    Ok((handle, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn duplicate_and_out_of_order_events_only_request_one_rescan() {
        let (rescans, mut output) = mpsc::channel(8);
        let (handle, task) = spawn_network_event_monitor(
            7,
            Duration::from_millis(30),
            Duration::from_secs(60),
            8,
            rescans,
        )
        .unwrap();
        assert_eq!(output.recv().await.unwrap().reason, RescanReason::Initial);
        handle
            .try_notify_with_generation(6, NetworkEventKind::Ipv4)
            .unwrap();
        handle.try_notify(NetworkEventKind::Link).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        handle.try_notify(NetworkEventKind::Ipv4).unwrap();
        let request = tokio::time::timeout(Duration::from_millis(80), output.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request,
            RescanRequest {
                generation: 7,
                reason: RescanReason::DebouncedNotification,
            }
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), output.recv())
                .await
                .is_err()
        );
        drop(handle);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn reconcile_runs_without_notifications() {
        let (rescans, mut output) = mpsc::channel(4);
        let (handle, task) = spawn_network_event_monitor(
            3,
            Duration::from_secs(1),
            Duration::from_millis(20),
            4,
            rescans,
        )
        .unwrap();
        assert_eq!(output.recv().await.unwrap().reason, RescanReason::Initial);
        assert_eq!(output.recv().await.unwrap().reason, RescanReason::Reconcile);
        drop(handle);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_last_handle_cancels_monitor() {
        let (rescans, mut output) = mpsc::channel(4);
        let (handle, task) = spawn_network_event_monitor(
            1,
            Duration::from_secs(1),
            Duration::from_secs(60),
            4,
            rescans,
        )
        .unwrap();
        output.recv().await.unwrap();
        let clone = handle.clone();
        drop(handle);
        assert!(!task.is_finished());
        drop(clone);
        tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .unwrap()
            .unwrap();
    }
}
