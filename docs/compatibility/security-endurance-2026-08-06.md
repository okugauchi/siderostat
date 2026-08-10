# Security and endurance record — 2026-08-06

対象branchは`rewrite/mode-aware`。この記録はmodel/GGUFを使わないagent gateと、実機operator gateを分離する。

## Agent gate

| Criterion | Result | Evidence |
|---|---|---|
| Secret fileは32 bytes以上、`0600` | PASS | `config::tests::rejects_insecure_or_duplicate_secret_files` |
| HMAC field改変、clock skew、nonce replay拒否 | PASS | `cluster::auth::tests::*` |
| Wrong interface/subnet/address/route candidate拒否 | PASS | `cluster::discovery::tests::rejects_self_interface_subnet_address_and_route_failures` |
| Peer ingress wrong source/token/hop拒否 | PASS | `proxy::tests::peer_security_rejects_wrong_source_token_and_hop` |
| Unknown/reused PIDへsignalしない | PASS | `cluster::process::tests::pid_reuse_and_unknown_process_never_reach_signaler` |
| Cancellation storm後in-flight=0 | PASS | `admission::tests::cancellation_storm_returns_in_flight_to_zero` |
| Streaming client cancellationでpermit解放 | PASS | `proxy::tests::mode_aware_proxy_releases_permit_on_client_cancellation` |
| State fileはsecret/tokenを保存しない | PASS | `cluster::state_store::tests::lock_is_exclusive_and_json_has_no_secret_or_token_field` |
| Fake child crash/restart、listener/orphanなし | PASS | `tests/phase3_supervisor.rs` |
| Route detach/attach 10 cycles | PASS | `tests/phase5_security.rs` |
| Fake distributed promotion/demotion 10 cycles | PASS | `tests/phase4_distributed.rs` |
| Header/body/tokenをlogしない | PASS | `metrics::tests::request_log_schema_excludes_headers_and_body`とchild raw line非出力 |

## Operator gate

| Criterion | Result | Blocker / procedure |
|---|---|---|
| Thunderbolt cable着脱10回、event-driven rescan | BLOCKED | 2台の対象Macと物理cableが必要。仕様第32.5節どおり実施する。 |
| RunAtLoad、proxy restart、single owner | PASS | MacBook ProとMac Studioで旧DS4/proxy labelをdisable/unloadしてplistを退避し、P5-04標準`local.siderostat.runtime`をinstall/bootstrap。`RunAtLoad=true`（login再現の代替静的検証）、`KeepAlive=true`、`ThrottleInterval=10`、absolute args、`kickstart -k`後のrunning/last exit 0、proxy 1、owned DS4 child 1、旧PID/orphanなし、health/readiness/doctor復帰を両nodeで確認。 |
| 実DS4 HELLO、route、short prompt、8K prefill | BLOCKED | ユーザー指定によりGGUFを使う検証は後で手動実施する。 |
| Memory pressure/startup/2-hop p50 | BLOCKED | 実modelと対象M4 Max/M5 Max topologyが必要。 |

Agent gateではsecret、model、runtime stateを生成物へ含めていない。Operator gateのBLOCKED項目をproduction enable前にPASSへ更新する。
