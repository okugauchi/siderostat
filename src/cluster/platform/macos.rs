use crate::cluster::{NetworkEventHandle, NetworkEventKind};
use anyhow::{Context, Result, anyhow};
use std::{sync::mpsc, thread};
use system_configuration::{
    core_foundation::{
        array::CFArray,
        runloop::{CFRunLoop, kCFRunLoopCommonModes},
        string::CFString,
    },
    dynamic_store::{SCDynamicStore, SCDynamicStoreBuilder, SCDynamicStoreCallBackContext},
};

struct CallbackContext {
    interface: String,
    events: NetworkEventHandle,
}

pub struct MacOsDynamicStoreWatcher {
    run_loop: CFRunLoop,
    thread: Option<thread::JoinHandle<()>>,
}

impl MacOsDynamicStoreWatcher {
    pub fn start(interface: &str, events: NetworkEventHandle) -> Result<Self> {
        let interface = interface.to_string();
        let (ready, receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("ds4-network-events".into())
            .spawn(move || run_dynamic_store(interface, events, ready))
            .context("spawn Dynamic Store run loop")?;
        let run_loop = receiver
            .recv()
            .context("Dynamic Store watcher stopped during startup")?
            .map_err(|message| anyhow!(message))?;
        Ok(Self {
            run_loop,
            thread: Some(thread),
        })
    }
}

impl Drop for MacOsDynamicStoreWatcher {
    fn drop(&mut self) {
        self.run_loop.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_dynamic_store(
    interface: String,
    events: NetworkEventHandle,
    ready: mpsc::SyncSender<Result<CFRunLoop, String>>,
) {
    let callback_context = SCDynamicStoreCallBackContext {
        callout: dynamic_store_changed,
        info: CallbackContext {
            interface: interface.clone(),
            events,
        },
    };
    let Some(store) = SCDynamicStoreBuilder::new("siderostat-network-events")
        .callback_context(callback_context)
        .build()
    else {
        let _ = ready.send(Err("create SCDynamicStore session failed".into()));
        return;
    };
    let keys = CFArray::from_CFTypes(&[CFString::from("State:/Network/Interface")]);
    let interface_pattern = format!("State:/Network/Interface/{interface}/(Link|IPv4)");
    let patterns = CFArray::from_CFTypes(&[
        CFString::from(interface_pattern.as_str()),
        CFString::from("Setup:/Network/Service/.*/(Interface|IPv4)"),
    ]);
    if !store.set_notification_keys(&keys, &patterns) {
        let _ = ready.send(Err(
            "register SCDynamicStore notification keys failed".into()
        ));
        return;
    }
    let Some(source) = store.create_run_loop_source() else {
        let _ = ready.send(Err("create SCDynamicStore run loop source failed".into()));
        return;
    };
    let run_loop = CFRunLoop::get_current();
    // SAFETY: `kCFRunLoopCommonModes` is an immutable Core Foundation static constant used as a
    // valid run-loop mode for the source registration.
    run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
    if ready.send(Ok(run_loop.clone())).is_err() {
        return;
    }
    CFRunLoop::run_current();
    // SAFETY: this is the same immutable Core Foundation mode used when registering the source.
    run_loop.remove_source(&source, unsafe { kCFRunLoopCommonModes });
}

#[allow(clippy::needless_pass_by_value)]
fn dynamic_store_changed(
    _store: SCDynamicStore,
    changed_keys: CFArray<CFString>,
    context: &mut CallbackContext,
) {
    for key in changed_keys.iter() {
        if let Some(kind) = classify_key(&key.to_string(), &context.interface) {
            let _ = context.events.try_notify(kind);
        }
    }
}

fn classify_key(key: &str, interface: &str) -> Option<NetworkEventKind> {
    if key == "State:/Network/Interface" {
        Some(NetworkEventKind::InterfaceList)
    } else if key == format!("State:/Network/Interface/{interface}/Link") {
        Some(NetworkEventKind::Link)
    } else if key == format!("State:/Network/Interface/{interface}/IPv4") {
        Some(NetworkEventKind::Ipv4)
    } else if key.starts_with("Setup:/Network/Service/") {
        Some(NetworkEventKind::Setup)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_only_required_dynamic_store_keys() {
        assert_eq!(
            classify_key("State:/Network/Interface", "bridge0"),
            Some(NetworkEventKind::InterfaceList)
        );
        assert_eq!(
            classify_key("State:/Network/Interface/bridge0/Link", "bridge0"),
            Some(NetworkEventKind::Link)
        );
        assert_eq!(
            classify_key("State:/Network/Interface/bridge0/IPv4", "bridge0"),
            Some(NetworkEventKind::Ipv4)
        );
        assert_eq!(
            classify_key("Setup:/Network/Service/ABC/IPv4", "bridge0"),
            Some(NetworkEventKind::Setup)
        );
        assert_eq!(
            classify_key("State:/Network/Interface/en0/IPv4", "bridge0"),
            None
        );
    }
}
