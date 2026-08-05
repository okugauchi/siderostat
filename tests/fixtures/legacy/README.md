# Legacy load-balancer behavior baseline

このfixtureはmode-aware architectureへの移行前に、commit `b66ba1c` の既存behaviorを比較可能な形で保存する。request body、prompt、secret、runtime logは含めない。

## Config parse

- `config::tests::parses_defaults_and_new_schema`
- `config::tests::parses_duration_units`
- `config::tests::parses_repository_example`
- `config::tests::rejects_duplicate_backend_ids`

入力fixtureは [`ds4-smart-proxy.example.toml`](ds4-smart-proxy.example.toml) を使用する。

## Public proxy streaming

- `proxy::tests::client_cancellation_releases_in_flight_permit`
- `proxy::tests::first_byte_timeout_is_not_retried`
- `proxy::tests::forwards_body_larger_than_replay_limit_without_retry_buffer`
- `proxy::tests::forwards_unknown_path_query_and_forwarding_headers`
- `proxy::tests::streams_chunks_without_buffering_whole_response`

## Admin readiness

専用testはbaseline時点では存在しない。`src/main.rs`に`ready` handlerは存在するが、admin listener経由のstatus/bodyを固定するtestはない。

## Baseline result

- Date: 2026-08-06
- Rust: `rustc 1.96.1 (31fca3adb 2026-06-26)`
- Command: `cargo test --all-targets`
- Result: 32 passed, 0 failed, 0 ignored
- Note: restricted sandbox内ではloopback bindが`PermissionDenied`となる8件を、sandbox外で再実行して成功を確認した。
