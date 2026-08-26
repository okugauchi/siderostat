# Installing Siderostat

For the Japanese guide, see [docs/installation.ja.md](installation.ja.md).

## Requirements

- Two Apple silicon Macs running a supported macOS version.
- Rust 1.85 or later on both Macs.
- A Thunderbolt cable and Thunderbolt networking enabled on both Macs.
- The compatible inference service and model obtained from an approved source.
- The same reviewed Siderostat source revision on both Macs.

Keep the Macs awake while the first build and readiness checks complete.

## Build and install

On each Mac, open a terminal in the Siderostat source checkout and run:

```sh
cargo xtask fingerprint-models
cargo xtask install --start
```

The first command records the model fingerprints used by the local configuration. The second command
builds the runtime and menu bar monitor, installs the user services, and starts them.

Do not run a second Siderostat installation method on the same Mac. The source installer owns the runtime,
the menu bar monitor, and their user services.

Repeat the commands on the other Mac using the same source revision and compatible model configuration.
When both Macs show a normal standalone state, connect the Thunderbolt cable and wait for the states to
progress through pairing to distributed operation.

## Updating

Update the source checkout to the reviewed revision, then run the same commands again:

```sh
cargo xtask fingerprint-models
cargo xtask install --start
```

The update keeps configuration, authentication data, model files, runtime state, and cache data. Do not
delete those files to make an update work. If the new source revision is not compatible with the existing
configuration or model, Siderostat fails closed and keeps the Mac in standalone operation.

## Rolling back

Select the previously reviewed source revision and run the normal source installation commands again.
Confirm standalone readiness before reconnecting the Macs. Do not mix binaries built from different source
revisions in a distributed pair.

## Uninstalling

From the Siderostat source checkout, run:

```sh
cargo xtask uninstall
```

This stops and disables the user services installed by the source workflow. It preserves configuration,
authentication data, model files, runtime state, and cache data. If the command reports an error, resolve
the stated condition and run it again; do not delete the preserved data or stop unrelated processes.

## Confirming the installation

The menu bar monitor should remain visible after `cargo xtask install --start` completes. The normal state
sequence is:

1. `Solo Standalone` while the other Mac is unavailable.
2. `Paired Standalone` after the Macs authenticate each other.
3. `Distributed (layer-parallel)` after distributed operation is ready.

If distributed operation is not safe, Siderostat keeps each Mac in standalone operation. This is expected
safety behavior and does not require another copy of the service.
