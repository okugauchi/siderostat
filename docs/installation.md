# Installing Siderostat

For the Japanese guide, see [docs/installation.ja.md](installation.ja.md).

## Requirements

- Two Apple silicon Macs running a supported macOS version.
- A Thunderbolt cable and Thunderbolt networking enabled on both Macs.
- The compatible inference service and model obtained from an approved source.
- The same `Siderostat-0.3.4.pkg` artifact on both Macs.

Keep the Macs awake while the first build and readiness checks complete.

## Build and install

Build the package once from the reviewed source revision:

```sh
cargo xtask app-dev --version 0.3.4 --build-number <monotonic-build> --verify
cargo xtask sign \
  --app-dir build/app-dev \
  --version 0.3.4 \
  --build-number <monotonic-build> \
  --application-identity "Developer ID Application: <name> (<team>)" \
  --installer-identity "Developer ID Installer: <name> (<team>)" \
  --notary-profile siderostat-notary \
  --with-dmg \
  --output-dir dist/release-0.3.4
```

Copy `dist/release-0.3.4/Siderostat-0.3.4.pkg` unchanged to both Macs. On each Mac,
double-click the package so macOS Installer performs the installation, then complete the administrator
prompt. The installer launches the application after installation; the application registers its own
bundle-contained runtime helper and menu-bar login item through Service Management.

The release artifact has a Developer ID signature, Apple secure timestamp, notarization, and a stapled
ticket. If a Mac still has the legacy source installation, run `cargo xtask uninstall` once from
that checkout before opening the package; preserved configuration, secrets, models, runtime state, and
cache are not removed.

When both Macs show a normal standalone state, connect the Thunderbolt cable and wait for the states to
progress through pairing to distributed operation.

## Updating

Create the new package from the reviewed revision and open it with macOS Installer on both Macs. Do not
use `cargo xtask install` for a package installation. The package keeps configuration, authentication
data, model files, runtime state, and cache data. Do not delete those files to make an update work. If the new package is not compatible with the existing
configuration or model, Siderostat fails closed and keeps the Mac in standalone operation.

## Rolling back

Open the previously reviewed package with macOS Installer on both Macs. Confirm standalone readiness before
reconnecting the Macs. Do not mix packages built from different source revisions in a distributed pair.

## Uninstalling

From the Siderostat source checkout, run:

```sh
cargo xtask uninstall
```

This stops and disables the legacy user services installed by the source workflow. It preserves configuration,
authentication data, model files, runtime state, and cache data. If the command reports an error, resolve
the stated condition and run it again; do not delete the preserved data or stop unrelated processes.

## Confirming the installation

The menu bar monitor should remain visible after Installer completes. The normal state
sequence is:

1. `Solo Standalone` while the other Mac is unavailable.
2. `Paired Standalone` after the Macs authenticate each other.
3. `Distributed (layer-parallel)` after distributed operation is ready.

If distributed operation is not safe, Siderostat keeps each Mac in standalone operation. This is expected
safety behavior and does not require another copy of the service.
