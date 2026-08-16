# siderostat

siderostat は、DS4のHTTP endpointを透過的にstreaming中継し、単一nodeのstandalone実行と、2 nodeのThunderbolt直結によるMXFP4 distributed実行を、ひとつのsupervisorで管理するRust製のmode-aware reverse proxy / cluster supervisorです。

転送先はmodeだけで一意に決まります。負荷やsession IDで変更しません。公開proxy processとlisten portは、standalone / distributedのmode切替中も維持されます。

## 主な機能

- OpenAI互換pathと未知pathの透過streaming中継（bodyをbufferしない）
- 3 mode: Solo Standalone、Paired Standalone、Distributed MXFP4
- peer不在時はlocal standalone profileへ転送し、peer存在時はcoordinatorへ集約
- standalone profileとしてQ2、Q2-Q4、MXFP4を選択でき、resident常駐またはDS4のSSD streamingを利用
- resident StandaloneでDSpark support GGUFを型付き設定し、fingerprintと実activation logをfail closedで検証
- compatibleなpeerが揃った場合、実DS4 worker HELLOとcomplete route確認の後、MXFP4 distributedへ自動昇格
- worker/route喪失時にstandalone profileへ自動降格
- standaloneとdistributedのKV cacheを分離
- proxy admissionとDS4 process drainを連動し、mode切替時に新規admissionを閉じる
- DS4 source/protocol contract、承認済みbinary集合、model、checkpoint、argvの互換性をfail closedで検証
- LaunchAgent、local CLI、構造化log、Prometheus互換metrics、loopback admin APIを提供
- macOSデスクトップ通知で起動・standalone再起動・distributed状態遷移を可視化（`[notifications]`で無効化可）
- macOSメニューバー常駐モニター（`monitor/` crate）でDS4のprefill progress・KV cache hitを可視化（設計中）
- 外部databaseまたはcluster state serviceなしで動作し、永続化するのはcluster lifecycle stateだけ

## モードとトポロジ

初期topologyはThunderbolt Bridgeで直結した2 nodeだけです。

| node | `bridge0` IPv4 | 役割 |
|---|---|---|
| coordinator | `10.99.0.1` | standalone実行、peer ingress、rendezvous、distributed coordinator |
| worker | `10.99.0.2` | standalone実行、distributed worker |

Roleは`bridge0`のIPv4から自動判定します。その他、未設定、競合はunknownとして、cluster listenerを開始しません。Roleはconfigで指定しません。

- クライアントは各nodeのpublic ingress `127.0.0.1:18080`（既定）へ接続します。
- Solo Standalone: 両nodeとも自身のlocal upstream `127.0.0.1:8000`（既定）へ転送します。
- Paired Standalone / Distributed MXFP4: workerのrequestはcoordinatorのpeer ingress `10.99.0.1:18082`（既定）へ転送され、coordinatorのupstreamで処理されます。
- Cluster controlは `10.99.0.1:9920 <-> 10.99.0.2:9920`（既定）。DS4 distributed native protocolはcoordinator `10.99.0.1:9911`（既定）で受けます。
- DS4 HTTP endpointは両nodeともloopbackにbindし、Thunderbolt Bridgeまたは通常LANへ公開しません。

Peer presentは、`bridge0`の期待address、`bridge0` scoped route、HMAC認証済みnode descriptor、有効なcontrol lease、`required_peer_stability`（既定5秒）の継続をすべて満たす状態だけです。Bonjour結果やICMPだけではpeer presentにしません。

## プロファイル

| standalone profile | residency | DS4起動形態 |
|---|---|---|
| Q2 | resident または ssd-streaming | HTTP server |
| Q2-Q4 | resident または ssd-streaming | HTTP server |
| MXFP4 | resident または ssd-streaming | HTTP server |

`ssd-streaming`はDS4の`--ssd-streaming`を意味します。model variantと混同しません。`resident`ではSSD streaming optionを生成しません。Standalone profileはnode固有でよく、coordinatorとworkerのmodel variant、residency、tuning値の一致をpairing条件にしません。

DSparkは現行DS4ではresident Standalone限定です。`[ds4.dspark]`の型付き設定から`--mtp`、`--dspark`と任意のconfidence/strictだけを生成します。Support GGUFのSHA-256/sizeはStandalone manifestとchild spawn前に照合し、DS4 activation eventをreadiness期限内に確認できなければ起動を失敗させます。Pathとfull digestはadmin response/logへ出しません。

Distributed profileはstandalone profileと独立です。初期実装では`distributed-mxfp4`を使い、coordinatorがHTTP + distributed coordinator、workerがdistributed worker（HTTPなし）です。

- coordinator layers: `0:19`、worker layers: `20:output`（既定）。gap、overlap、layer 0欠落、output head欠落は拒否します。
- Distributedに用いる両nodeのMXFP4 content SHA-256を一致させます。
- Standaloneとdistributedは異なる`--kv-disk-dir`を必須とし、同じGGUFを使う場合も共有しません。

## クイックスタート

Rust stable（edition 2024）が必要です。実DS4 binaryとmodelを使う準備・検証は [`docs/installation.md`](docs/installation.md) に従います。

```bash
cargo build --release
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

`.cargo/config.toml` はRust test harnessの `RUST_TEST_THREADS` を `1` に固定する。
reconnect production testは共有loopback resourceとfake lifecycleを使うため、CIとlocalの
`cargo test`を同じ直列スケジュールに揃え、並列実行時のタイミング依存を避ける。

設定はTOMLで、`siderostat.example.toml`が配布用の完全例です。探索順は次のとおりです。

1. `--config PATH`
2. `SIDEROSTAT_CONFIG`
3. `./siderostat.toml`
4. platform既定path

Secret/token fileは各32 bytes以上、mode `0600`、相互に異なるpathで配置します。Control secretとpeer proxy tokenはそれぞれ両nodeで同じ値を使い、admin tokenはnodeごとに生成します。Control、peer proxy、adminの3用途の間で値またはfileを流用しません。`openssl rand`などで生成し、configにはfile pathだけを書きます。

起動形式です。Subcommandなしは`serve`と同じです。

```bash
siderostat --config ./node.toml
# または
siderostat serve --config ./node.toml
```

起動後、loopback admin APIで確認します。

```bash
curl --fail --silent http://127.0.0.1:18081/healthz
curl --fail --silent http://127.0.0.1:18081/readyz
curl --fail --silent http://127.0.0.1:18081/cluster
curl --fail --silent http://127.0.0.1:18081/metrics
```

CLIのcluster commandはrunning processのadmin API clientであり、別supervisorを起動しません。

```bash
siderostat cluster status
siderostat cluster status --json
siderostat cluster doctor
siderostat cluster doctor --json
siderostat cluster reconcile
siderostat cluster pair
siderostat cluster promote
siderostat cluster demote
siderostat cluster demote --reason "operator-requested"
siderostat cluster restart
siderostat cluster fingerprint --profile standalone
siderostat cluster fingerprint --profile distributed
```

Mutation（pair/promote/demote/restart/fingerprint/reconcile）はloopbackでもadmin token必須です。Status/doctorはread-onlyです。`promote`は実HELLO/compatibility条件を迂回しません。

## セキュリティ

- Public/admin listenerはloopback既定。peer ingress/control/DS4 distributedはThunderbolt Bridgeだけにbindします。
- Peer ingressはsource IP、token、hopを検証します。Control planeはHMAC、timestamp、nonce、source IPを検証します。
- Admin mutationはtoken必須です。
- Secret/token fileは32 bytes以上、mode `0600`。Control、peer proxy、adminの3用途の間で値またはfileを流用しません。
- DS4 childをTokio process APIでspawnし、shellを介しません。Unknown processをkillしません。
- Model fingerprint時にregular file/canonical pathを確認します。書換可能なsymlinkをmodel pathに使いません。
- Authorization/API key、request/response body、prompt、session/conversation ID、peer proxy token、HMAC secret、完全model digest/deployment IDをlogしません。

DS4 native distributed trafficとpeer proxy bodyは暗号化されません。専用の物理Thunderbolt linkを信頼境界とします。

## 制限事項

- standalone/distributed切替中は503の短いwindowがあります。
- Workerからcoordinatorへのrequestはproxyを2 hop通ります。
- Peer data/DS4 native trafficは暗号化されません。
- 2 node固定です。3 node以上の任意topologyは対象外です。
- Peer discoveryは自動ですが、role addressは`10.99.0.1` / `10.99.0.2`固定で、DHCP based electionは行いません。
- standalone/distributed間でlive KVを引き継ぎません。mode切替後はtranscriptから再構築します。
- DS4 log textへの依存があります。
- MXFP4 SSD streaming on MetalとMXFP4 distributedのproduction可否は、実DS4/modelを使うactual acceptance結果で決めます。
- 現行DS4はsupport GGUFをdistributed roleでloadしないため、DSparkはMXFP4 Distributedへの昇格中には適用されません。

## 関連文書

- [`docs/spec.md`](docs/spec.md): 完全な仕様と受け入れ条件
- [`docs/installation.md`](docs/installation.md): 実DS4/modelを使う導入ガイド
- [`docs/operations.md`](docs/operations.md): 運用ガイド（status、doctor、logs、metrics、manual state、restart、rollback）
- [`docs/troubleshooting.md`](docs/troubleshooting.md): failure symptom別の診断手順
- [`docs/compatibility/ds4-b030961.md`](docs/compatibility/ds4-b030961.md): v0.1.0 DS4 compatibility記録
- [`docs/compatibility/security-endurance-2026-08-06.md`](docs/compatibility/security-endurance-2026-08-06.md): security/endurance gate記録
- [`docs/compatibility/documentation-clean-install-2026-08-10.md`](docs/compatibility/documentation-clean-install-2026-08-10.md): P6-05導入文書検証記録
- [`docs/releases/v0.1.0.md`](docs/releases/v0.1.0.md): v0.1.0 release notes
- [`docs/releases/v0.1.0-acceptance.md`](docs/releases/v0.1.0-acceptance.md): final acceptanceとrelease artifact checksum
- [`docs/menu-bar-monitor-spec.md`](docs/menu-bar-monitor-spec.md): メニューバーモニターの仕様
- [`siderostat.example.toml`](siderostat.example.toml): 配布用config例
- [`contrib/launchd/README.md`](contrib/launchd/README.md): macOS LaunchAgentのinstall/verify/uninstall
