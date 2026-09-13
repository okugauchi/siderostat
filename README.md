# siDeroStat

日本語版: [README.ja.md](README.ja.md)

siDeroStat lets two Apple silicon Macs work together as a two-node inference setup. The Macs are
connected by Thunderbolt, and siDeroStat changes between local and distributed operation as the
connection becomes ready or unavailable. It builds on the DS4 inference service (DwarfStar) and keeps
the inference service, its model, and your credentials local.

> [!NOTE]
> The currently verified model is DeepSeek V4 Flash. Other models are not supported unless a release
> explicitly says otherwise.

> [!NOTE]
> siDeroStat supports exactly two Macs connected through Thunderbolt networking. Three or more Macs
> are not supported.

## Features

- Operate each Mac locally when the other Mac is unavailable.
- Detect the connection and authentication state of both Macs.
- Move to distributed operation after both Macs are ready.
- Return to local operation when the connection or other Mac is unavailable.
- Choose the connection mode from the menu bar: `Automatic` follows the
  connection state, and `ForcedStandalone` keeps this Mac local even while
  the peer is visible.
- Run Mac-to-Mac tensor parallelism (TP) over the Thunderbolt connection when
  both Macs and the model support it. TP is gated by the selected connection
  mode, peer protocol negotiation, and upstream capability; it is never
  started against an unsupported peer.
- Manage the DS4 source, build, download, model, and activation lifecycle from
  the manager view. Fetching, building, and downloading never change the
  currently active artifact, and a failed startup can be rolled back to the
  previous active artifact.
- Provide an optional Web Search Bridge backed by SearXNG. Search is off by
  default, external access is opt-in, and the bridge never runs the
  inference service's normal request queue.
- Start, stop, and restart the managed inference service from the menu bar.
- Show operating state, readiness, and inference progress through the menu bar monitor and notifications.
- Keep inference content and credentials out of notifications and diagnostic output.

## Supported operating states

| State | Meaning |
|---|---|
| `Solo Standalone` | This Mac is serving by itself. |
| `Paired Standalone` | The Macs are connected and authenticated, but distributed operation is not ready. |
| `Distributed (layer-parallel)` | Both Macs are cooperating on an inference. |

The connection mode (`Automatic` / `ForcedStandalone`) is independent of these operating states:
`ForcedStandalone` keeps the Mac in standalone operation even while the peer stays visible, and
`Automatic` returns to following the connection state.

`MXFP4` is model quantization information. `DSpark` is speculative-execution support information.
They are model details, not operating-state or topology names.

## Requirements

- Two Apple silicon Macs with a supported macOS version.
- Rust 1.85 or later on each Mac.
- A Thunderbolt cable and Thunderbolt networking enabled on both Macs.
- A compatible inference service and model obtained from an approved source.

## Installation

Install the same reviewed source revision on both Macs. From the repository checkout on each Mac:

```sh
cargo xtask fingerprint-models
cargo xtask install --start
```

The command builds the local runtime and menu bar monitor, installs the user services, and starts them.
Connect the Thunderbolt cable after both Macs reach a normal standalone state.

For the complete procedure, see the [installation guide](docs/installation.md).

## Using siDeroStat

Use this local OpenAI-compatible endpoint in your client application:

```text
http://127.0.0.1:18080/v1
```

The menu bar monitor shows the current state and progress. During startup or a state change, a request
may temporarily fail with HTTP 503 or HTTP 504. siDeroStat does not replay a failed request, so the
client application must decide whether a retry is safe.

### Connection mode

From the menu bar you can choose the connection mode:

- `Automatic` — siDeroStat follows the connection state: it moves to
  distributed operation when the peer becomes ready and returns to standalone
  when it is unavailable. This is the default.
- `ForcedStandalone` — siDeroStat keeps this Mac local even while the peer
  is visible. No TP, pairing, or promotion is started while this mode is
  selected, and the choice is preserved across restarts.

The selected mode is applied to the cluster through the operation-policy API; it is not a display-only
setting. While a policy change is in progress, the menu shows the pending job and any busy reason.

### DS4 Manager

The manager view lets you manage the DS4 source and model lifecycle:

- Fetch a reviewed DS4 source commit, build the runtime, and record its binary identity.
- Download a model with resume support. A model without a verified checksum cannot be activated.
- Activate a downloaded model. Activation is a single confirmed operation and does not change the
  currently active artifact until it succeeds; a failed startup can be rolled back to the previous
  active artifact. Rollback keeps the previous artifact available.
- Show build targets and failure reasons. Credentials and raw build logs are not shown in the
  manager view.

### Web Search Bridge

An optional SearXNG-backed Web Search Bridge is available for the Responses API. It is off by default,
external access is opt-in, and SearXNG is never installed or started automatically. Search is bounded
and does not run the inference service's normal request queue. The bridge state and SearXNG health are
shown in the monitor.

### Dry-run mode (development only)

`serve --dry-run` runs the node-to-node clustering (discovery, control plane, pairing, promotion,
demotion, recovery) without starting, stopping, or restarting a real ds4-server process. Startup
cleanup, restart reconcile, and persistent-state reads/writes are skipped. It is intended for
developing and validating the clustering logic itself.

```text
siderostat serve --dry-run
```

Dry-run mode is a development-only flag: it is never enabled by default and must be passed
explicitly. It does **not** serve real inference requests, so it must not be used in production.

## Limitations

- Only two Macs are supported.
- The Macs and model configurations must satisfy the compatibility requirements of the source revision.
- A short interruption can occur while the operating state changes or the inference service starts.
- Automatic degraded recovery is disabled by default. When enabled, recovery is bounded and does not
  bypass the inference service's normal request queue.
- Mac-to-Mac tensor parallelism is implemented and gated by the connection mode, peer negotiation,
  and upstream capability. Real-device verification of TP is pending the hardware/OS approval step; it
  is recorded as Pending until then and is never started against an unsupported peer.
- RDMA transport and distributed DSpark optimization are not reimplemented; TP follows the upstream
  DS4 main contract.
- `serve --dry-run` is a development-only clustering check and does not process real inference.

## End-user documentation

- [Installation guide](docs/installation.md) · [日本語](docs/installation.ja.md)
- [Operations guide](docs/operations.md) · [日本語](docs/operations.ja.md)
- [Troubleshooting guide](docs/troubleshooting.md) · [日本語](docs/troubleshooting.ja.md)
