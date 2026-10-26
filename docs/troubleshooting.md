# Troubleshooting Siderostat

For the Japanese guide, see [docs/troubleshooting.ja.md](troubleshooting.ja.md).

## The menu bar monitor does not appear

Open the reviewed `.pkg` with macOS Installer once. Do not launch the monitor executable from a terminal
or run a second Siderostat copy.

Wait for the runtime and monitor to start. If Installer reports an error, resolve that error and retry the
same package after confirming that no unrelated process is being stopped.

## The monitor says offline

Check the following in order:

1. Confirm that the `.pkg` installation completed on the Mac showing offline.
2. Confirm that the inference service has had time to start; the first start can take several minutes.
3. Confirm that the other Mac is awake and that the Thunderbolt cable is connected at both ends.
4. Confirm that Thunderbolt networking is enabled in System Settings.
5. Wait for the state to settle before restarting anything.

If the other Mac is unavailable, `Solo Standalone` is the expected safe state. Do not start another copy
of the service to clear an offline message.

## The Macs do not enter distributed operation

Both Macs must use the same reviewed source revision and compatible model configuration. The Macs must
first reach `Paired Standalone`; only then can they enter `Distributed (layer-parallel)`. If compatibility
checks fail, Siderostat deliberately remains in standalone operation.

Disconnect and reconnect the Thunderbolt cable once, then wait for both monitors to settle. Do not change
model files or delete runtime data while the monitors are transitioning.

## A request returns HTTP 503 or HTTP 504

This can happen while the inference service starts or while Siderostat changes operating state. Wait until
the menu bar monitor shows a ready state, then let the client application retry only if repeating the request
is safe. Siderostat does not automatically replay the failed request.

## Restart or recovery does not finish

Do not select restart repeatedly. Stop new work and wait for the current notification to finish. If the
notification requests manual recovery, contact the administrator and include the source revision, Mac model,
approximate time, and state shown in the menu bar. Do not include prompts, responses, API keys, passwords,
or authentication data.

## Login start is not working

Confirm that the `.pkg` installation completed and that Siderostat.app was opened. If macOS asks for a
login-item or background-item approval, open System Settings > General > Login Items and approve the
Siderostat entries. After changing an approval, reopen the same package-installed application.

## Updating or uninstalling reports an error

For package updates, open the reviewed `.pkg` with macOS Installer and resolve the displayed error before retrying.
Do not use `cargo xtask install` for the package-installed application.

Uninstalling uses:

```sh
cargo xtask uninstall
```

The package workflow and the legacy source uninstall preserve configuration, authentication data, model files,
runtime state, and cache data. Do
not use a generic cleanup command or manually delete those files.

## Information to provide when asking for help

Provide the source revision, Mac model, Thunderbolt connection state, the state shown on each menu bar monitor,
and the approximate time of the problem. Remove prompts, responses, credentials, API keys, personal paths,
and other private data before sharing screenshots or logs.

## TP does not become ready

Check the configured topology and transport first: TP requires `tensor-parallel` and `rdma` on both Macs.
The control Bridge connection and the RDMA member interface/address are separate. Confirm that the
selected device is active and that the configured numeric GID index maps to the member IPv4 address.
siDeroStat does not enable RDMA, assign addresses, or probe the TP data socket.

A schema-2 LP manifest, candidate/unknown TP capability, wrong source or role binary, changed model,
context mismatch, or distributed-help digest mismatch prevents TP startup. Prepare matching verified
schema-3 deployment/capability/role metadata; do not bypass the checks with extra arguments.
The worker needs `ds4` and the coordinator needs `ds4-server`. LP layers and DSpark/SSD settings must
be absent from TP arguments.

HTTP listening alone is insufficient: both children must stay alive, report the same-session RDMA
handshake, and complete the internal generation warm-up. A prepared worker whose coordinator abandons
startup is recovered after its bounded startup deadline. Failed drain/stop/fallback keeps admission
closed and requires manual intervention. Child diagnostics show the actual local TP lifecycle slot.

The failed-attempt budget survives fallback and automatic restarts. After three failures, resolve the
cause and use a manual runtime restart, apply ForcedStandalone then Automatic, or install a validated
new deployment/RDMA identity. Reconcile verifies standalone recovery without clearing the budget.
See [TP operation](operations.md#tensor-parallel-operation) for the acceptance boundary.
