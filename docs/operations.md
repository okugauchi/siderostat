# Operating Siderostat

This guide covers normal day-to-day use of Siderostat. For the Japanese guide, see
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
  start behavior. This does not remove Siderostat from the Login Items list.
- `Open Login Items`: opens System Settings > General > Login Items. It is safe to open this at any time.
- `Quit Siderostat`: quits only the menu bar application. It does not mean that user data has been deleted.

Use only one Siderostat menu bar application. Do not launch a second copy or create a separate service
for the inference process.

## Using the local API

Applications on the same Mac can use this OpenAI-compatible endpoint:

```text
http://127.0.0.1:18080/v1
```

The endpoint remains local to the Mac by default. During a state change or service startup, a request
may receive HTTP 503 or HTTP 504. Siderostat does not replay a failed request; the client application
must decide whether a retry is safe.

## Cable connection and distributed operation

When the Thunderbolt connection is attached, Siderostat checks the connection, authentication, model
compatibility, and service readiness in order. It does not treat a cable signal alone as proof that
distributed operation is safe.

When the cable is removed or the other Mac becomes unavailable, Siderostat returns to standalone
operation. Reconnect the cable and wait for the menu bar state to settle before starting a long-running
job.

## Recovery and canary checks

Siderostat can perform a bounded canary check when recovery is requested. The check verifies a
meaningful response and response time; it is limited in duration and does not bypass the inference
service's normal request queue. It does not include your prompts, responses, credentials, or API keys
in notifications or diagnostic output. A recovery attempt may also record a redacted diagnostic snapshot
for the administrator; it contains operational state only.

Automatic degraded recovery is disabled by default. When it is enabled by an administrator, it remains
bounded by an attempt limit and a cooldown period before another attempt is allowed. A failed recovery keeps admission closed rather than
repeatedly restarting the Macs. If a notification says that manual recovery is required, stop new batch
work, wait for any active request to finish, and contact the administrator instead of repeatedly selecting
restart.

Before starting an important long-running job, wait until the menu bar monitor shows the final ready
state on both Macs. If a recovery notification is still present, do not start the job.

## Safety and privacy

- Do not add a second Siderostat or inference-service login item.
- Do not delete files under Siderostat's application-support data while the service is running.
- Use the supplied uninstaller instead of manually deleting the app or stopping unrelated processes.
- Inference content, credentials, and API keys are not intended to appear in Siderostat notifications.
- The dedicated Thunderbolt link is the trust boundary for communication between the Macs.
