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
