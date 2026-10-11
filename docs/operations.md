# Operating siDeroStat

This guide covers normal day-to-day use of siDeroStat. For the Japanese guide, see
[docs/operations.ja.md](operations.ja.md).

## What the menu bar monitor shows

The menu bar monitor shows the current operating state, readiness, connection state, and current
inference progress. Throughput values describe the operation currently in progress and are not kept as
the current value after that operation ends.

When progress has not arrived for an extended period, the monitor may show a stale or stalled warning.
Treat that warning as a reason to pause new work and check the recovery guidance below.

The operating state describes how the Macs are working together:

| State | Meaning |
|---|---|
| `Solo Standalone` | This Mac is serving by itself. |
| `Paired Standalone` | The Macs are connected and authenticated, but distributed operation is not ready. |
| `Distributed (layer-parallel)` | Both Macs are cooperating on an inference. |

Model details are separate from the operating state. `MXFP4` describes model quantization; `DSpark`
describes speculative-execution support. Neither is a topology or operating-state name.

## Normal actions

The menu uses the following actions:

- `Restart siderostat-runtime`: restarts the managed inference service while preserving the current
  configuration. A short standalone or offline period can appear during the restart.
- `Start siderostat-runtime and enable automatic start`: starts the service and enables its background
  start behavior.
- `Stop siderostat-runtime and disable automatic start`: stops the service and disables its background
  start behavior. This does not remove siDeroStat from the Login Items list.
- `Open Login Items`: opens System Settings > General > Login Items. It is safe to open this at any time.
- `Quit siDeroStat`: quits only the menu bar application. It does not mean that user data has been deleted.

Use only one siDeroStat menu bar application. Do not launch a second copy or create a separate service
for the inference process.

## Using the local API

Applications on the same Mac can use this OpenAI-compatible endpoint:

```text
http://127.0.0.1:18080/v1
```

The endpoint remains local to the Mac by default. During a state change or service startup, a request
may receive HTTP 503 or HTTP 504. siDeroStat does not replay a failed request; the client application
must decide whether a retry is safe.

## Cable connection and distributed operation

When the Thunderbolt connection is attached, siDeroStat checks the connection, authentication, model
compatibility, and service readiness in order. It does not treat a cable signal alone as proof that
distributed operation is safe.

When the cable is removed or the other Mac becomes unavailable, siDeroStat returns to standalone
operation. Reconnect the cable and wait for the menu bar state to settle before starting a long-running
job.

## Connection mode

The connection mode is independent of the operating state and is chosen from the menu bar:

- `Automatic` — siDeroStat follows the connection state. This is the default. It moves to distributed
  operation when the peer becomes ready and returns to standalone when the peer is unavailable.
- `ForcedStandalone` — siDeroStat keeps this Mac local even while the peer stays visible. No TP,
  pairing, or promotion is started while this mode is selected, and the choice is preserved across
  restarts.

The selected mode is applied to the cluster through the operation-policy API, not as a display-only
setting. While a policy change is in progress, the menu shows the pending job and any busy reason.
Choosing `ForcedStandalone` before a long-running job keeps the Mac isolated from peer state changes.

## DS4 Manager

The manager view manages the DS4 source and model lifecycle without changing the running artifact:

- Fetch a reviewed DS4 source commit, build the runtime, and record its binary identity.
- Download a model with resume support. A model without a verified checksum cannot be activated.
- Activate a downloaded model. Activation is a single confirmed operation; the currently active artifact
  is not changed until activation succeeds. A failed startup can be rolled back to the previous active
  artifact, which is kept available.
- Build targets and failure reasons are shown. Credentials and raw build logs are not shown in the
  manager view.

Fetching, building, and downloading never interrupt the active inference service. Use the manager view
to stage a new model and confirm the active artifact after both Macs have the verified artifact.

## Web Search Bridge

An optional SearXNG-backed Web Search Bridge is available for the Responses API. It is off by default,
external access is opt-in, and SearXNG is never installed or started automatically. Search is bounded
and does not run the inference service's normal request queue. The bridge state and SearXNG health are
shown in the monitor.

## Recovery and canary checks

siDeroStat checks service responsiveness when recovery is requested. It does not include your prompts,
responses, credentials, or API keys in notifications or diagnostic output.

Automatic degraded recovery is disabled by default. When it is enabled by an administrator, it remains
bounded by an attempt limit and a cooldown period before another attempt is allowed. A failed recovery keeps admission closed rather than
repeatedly restarting the Macs. If a notification says that manual recovery is required, stop new batch
work, wait for any active request to finish, and contact the administrator instead of repeatedly selecting
restart.

Before starting an important long-running job, wait until the menu bar monitor shows the final ready
state on both Macs. If a recovery notification is still present, do not start the job.

## Safety and privacy

- Do not add a second siDeroStat or inference-service login item.
- Do not delete files under siDeroStat's application-support data while the service is running.
- Use `cargo xtask uninstall` instead of manually deleting files or stopping unrelated processes.
- Inference content, credentials, and API keys are not intended to appear in siDeroStat notifications.
