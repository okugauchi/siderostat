# Siderostat

Japanese version: [README.ja.md](README.ja.md)

Siderostat is software for building a distributed inference cluster from two Apple silicon Macs running DwarfStar's inference server, `ds4-server`, connected over Thunderbolt 5. It accepts requests from OpenAI-compatible API clients and operates as a proxy that changes request routing according to the Thunderbolt connection state.

A primary use case is a MacBook Pro paired with a Mac Studio. For example, while away from home, the MacBook Pro can run tasks locally with a Q2-Q4 quantized DeepSeek V4 Flash model. After returning home, connect the Mac Studio and MacBook Pro with a Thunderbolt 5 cable. Siderostat detects both Macs and switches to distributed inference with a quantized MXFP4 DeepSeek V4 Flash model, making the combined resources available for overnight batch processing. Disconnecting the cable returns the MacBook Pro to local operation with the Q2-Q4 model. You do not need to select the Mac Studio manually or change the inference endpoint.

> [!NOTE]
> The only model currently verified by Siderostat with `ds4-server` is DeepSeek V4 Flash. Other models, including GLM 5.2, are not supported.

> [!NOTE]
> The supported topology is exactly two Macs connected by Thunderbolt Bridge. Configurations with three or more Macs, or distributed processing over an arbitrary number of Macs, are not supported.

## Features

- Run `ds4-server` locally on each Mac and provide inference.
- Automatically check the connection, authentication, and operating state of both Macs.
- Switch to distributed inference automatically when the Macs are connected over Thunderbolt.
- Return to local operation when the Thunderbolt connection or peer node has a problem.
- Manage `ds4-server` start, stop, and restart operations.
- Display state and Prefill / Decode throughput through macOS notifications and a menu bar monitor.
- Never store inference content, credentials, or other secrets in logs.

## Supported two-Mac topology

The roles of the two Macs are determined statically from the fixed IPv4 addresses assigned to the virtual Bridge for the IP-over-Thunderbolt connection. One node coordinates connection management and distributed processing; the other performs part of the distributed processing.

| Role | Primary responsibility | Example Thunderbolt Bridge address |
|---|---|---|
| Coordinator | Manage the two-Mac connection and coordinate distributed processing | `10.99.0.1` |
| Worker | Perform part of the distributed processing | `10.99.0.2` |

You do not need to write the role directly into the configuration file. If an address is missing, duplicated, or unexpected, Siderostat safely keeps that Mac in local operation without starting the two-Mac features.

## Operating states

### Solo Standalone

Inference is performed only by that Mac's `ds4-server`, without using the peer node. This state continues to provide service when the peer is offline or the Thunderbolt connection is unplugged.

### Paired Standalone

The peer connection and authentication are complete, but distributed inference has not started yet. Siderostat continues to use local operation until distributed processing is ready.

### Distributed (layer-parallel)

The two `ds4-server` processes cooperate to handle one inference. The sample configuration uses an MXFP4 DeepSeek V4 Flash model. If Siderostat cannot switch to distributed inference, it returns to local operation.

During a state transition, new requests may be temporarily rejected with HTTP 503. Siderostat does not automatically retry a request that failed during a transition in another operating state.

## Requirements

- Two macOS Macs with Apple silicon
- A Thunderbolt 5 cable connecting the two Macs
- IP over Thunderbolt enabled in macOS
- The `ds4-server` executable
- A DeepSeek V4 Flash model file (downloadable from Hugging Face with `download_model.sh`) and a compatible `ds4-server` configuration
- A stable Rust toolchain for building and installing Siderostat

See the [installation guide](docs/installation.md) for the sources of `ds4-server` and the model, model compatibility, and how to verify the executable on each Mac.

## Installation

Perform the installation separately on both Macs. After placing the model, calculate its fingerprint once, then use `cargo xtask install` to install Siderostat (the proxy server), the macOS menu bar monitor, the configuration file, and the macOS login-start item.

```sh
cd /path/to/siderostat
cargo xtask fingerprint-models
cargo xtask install
```

`cargo xtask install` asks whether hashes for GGUF files larger than 80 GB should be recalculated. The default is not to recalculate them. Run `cargo xtask fingerprint-models` again whenever a model is updated or replaced.

The [installation guide](docs/installation.md) describes how to verify `ds4-server` on both Macs, place models, share authentication data, and verify distributed inference. To start the service as part of installation, use `cargo xtask install --start`.

The main files created by installation are:

- Configuration: `~/Library/Application Support/siderostat/config.toml`
- Authentication data: `~/Library/Application Support/siderostat/secrets/`
- Menu bar monitor configuration: `~/monitor.toml`
- macOS launch items: `~/Library/LaunchAgents/`

Restart Siderostat after installation if you change the configuration and want it to be reloaded.

## Usage and status checks

Use the following URL as the OpenAI-compatible API endpoint in client applications:

```text
http://127.0.0.1:18080/v1
```

Use the following commands to inspect the state:

```sh
siderostat cluster status
siderostat cluster doctor
curl --fail --silent http://127.0.0.1:18081/healthz
curl --fail --silent http://127.0.0.1:18081/readyz
```

`status` displays the current state, while `doctor` checks whether the system can accept inference. Neither command normally changes state.

## Notifications and menu bar monitor

The menu bar monitor displays:

- The current operating state
- Input preparation progress and throughput
- KV cache usage
- Generation throughput
- Whether the target node can accept inference

Input-preparation and generation throughput are shown for the currently active operation. The monitor does not keep displaying the last value after the operation completes.

macOS notifications report local operation, two-Mac connection, distributed inference, restart events, and states that require recovery.

## Security and communication scope

- The user-facing and administration APIs accept connections only from the same Mac by default.
- `ds4-server` is not exposed to the ordinary LAN.
- Management and distributed-processing traffic between the two Macs uses Thunderbolt Bridge.
- The authentication data is not an SSH private key or a PEM-format key. It is Siderostat-specific authentication data.
- Inference content, input content, credentials, and complete hashes are not stored in logs.

Distributed-processing traffic over Thunderbolt Bridge is not encrypted. Treat the dedicated physical connection as the trust boundary.

## Limitations

- Exactly two Macs are supported. Configurations with three or more Macs are not supported.
- The two Macs may use different Apple silicon generations, but their `ds4-server` executables and model configurations must satisfy the compatibility conditions verified during installation.
- The number of concurrent inference requests is limited. The default limit is two requests, but processing time and concurrent-execution stability vary with the model, input length, output length, and Mac memory.
- Requests may temporarily fail while the operating state changes or while `ds4-server` is starting.
- Requests are not retried automatically. The client application must decide whether to retry after receiving HTTP 503 or HTTP 504.

## Related documentation

- [Installation guide](docs/installation.md): install `ds4-server`, models, the two-Mac topology, and launch items
- [Operations guide](docs/operations.md): status checks, restart, recovery, and rollback
- [Troubleshooting guide](docs/troubleshooting.md): connection, authentication, distributed inference, and startup failures
- [Menu bar monitor specification](docs/menu-bar-monitor-spec.md): displayed information and configuration
- [Detailed specification](docs/spec.md): operating conditions, communication, compatibility, and security details
- [Developer guide](docs/development.md): builds, tests, and static checks
- [Example configuration](siderostat.example.toml): configuration template for installation
