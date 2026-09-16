# Siderostat

日本語版: [README.ja.md](README.ja.md)

Siderostat lets two Apple silicon Macs work together as a two-node inference setup. The Macs are
connected by Thunderbolt, and Siderostat changes between local and distributed operation as the
connection becomes ready or unavailable.

> [!NOTE]
> The currently verified model is DeepSeek V4 Flash. Other models are not supported unless a release
> explicitly says otherwise.

> [!NOTE]
> Siderostat supports exactly two Macs connected through Thunderbolt networking. Three or more Macs
> are not supported.

## Features

- Operate each Mac locally when the other Mac is unavailable.
- Detect the connection and authentication state of both Macs.
- Move to distributed operation after both Macs are ready.
- Return to local operation when the connection or other Mac is unavailable.
- Start, stop, and restart the managed inference service from the menu bar.
- Show operating state, readiness, and inference progress through the menu bar monitor and notifications.
- Keep inference content and credentials out of notifications and diagnostic output.

## Supported operating states

| State | Meaning |
|---|---|
| `Solo Standalone` | This Mac is serving by itself. |
| `Paired Standalone` | The Macs are connected and authenticated, but distributed operation is not ready. |
| `Distributed (layer-parallel)` | Both Macs are cooperating on an inference. |

`MXFP4` is model quantization information. `DSpark` is speculative-execution support information.
They are model details, not operating-state or topology names.

## Requirements

- Two Apple silicon Macs with a supported macOS version.
- A Thunderbolt cable and Thunderbolt networking enabled on both Macs.
- A compatible inference service and model obtained from an approved source.

## Installation

Install the same `Siderostat-0.3.4.pkg` artifact on both Macs by opening it in the macOS
Installer application and completing the administrator prompt. The package installs the official
`Siderostat.app` bundle; do not run `cargo xtask install` on these Macs.

If a Mac still has the legacy source installation, run `cargo xtask uninstall` once from that source
checkout before opening the package. This preserves configuration, secrets, models, runtime state, and cache.

The package is signed with Developer ID, has an Apple secure timestamp, and is notarized for Gatekeeper-ready
distribution. It is suitable for the two-node rollout described by this release.

The package can be built from the reviewed source revision with the developer workflow documented in
the [development guide](docs/development.md). It is then copied unchanged to the other Mac.

Connect the Thunderbolt cable after both Macs reach a normal standalone state.

For the complete procedure, see the [installation guide](docs/installation.md).

## Using Siderostat

Use this local OpenAI-compatible endpoint in your client application:

```text
http://127.0.0.1:18080/v1
```

The menu bar monitor shows the current state and progress. During startup or a state change, a request
may temporarily fail with HTTP 503 or HTTP 504. Siderostat does not replay a failed request, so the
client application must decide whether a retry is safe.

## Limitations

- Only two Macs are supported.
- The Macs and model configurations must satisfy the compatibility requirements of the source revision.
- A short interruption can occur while the operating state changes or the inference service starts.
- Automatic degraded recovery is disabled by default. When enabled, recovery is bounded and does not
  bypass the inference service's normal request queue.
- Mac-to-Mac tensor parallelism, RDMA transport, and distributed DSpark are not supported.

## End-user documentation

- [Installation guide](docs/installation.md) · [日本語](docs/installation.ja.md)
- [Operations guide](docs/operations.md) · [日本語](docs/operations.ja.md)
- [Troubleshooting guide](docs/troubleshooting.md) · [日本語](docs/troubleshooting.ja.md)
