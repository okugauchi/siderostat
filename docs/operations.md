# siderostat 運用ガイド

この文書は、`docs/spec.md`と実装済みのadmin API / CLI / logging / metricsに基づき、siderostatの運用手順を定める。実DS4 binaryとmodelを使う導入は [`docs/installation.md`](installation.md) を参照する。

## 1. Status確認

Running processの状態を `cluster status` または `GET /cluster` で確認する。CLIはadmin API clientであり、別supervisorを起動しない。

```sh
siderostat cluster status
siderostat cluster status --json
curl --fail --silent http://127.0.0.1:18081/cluster
```

`/cluster`は次の項目を返す（spec第23.1節）。

- node_id、role、mode、state、generation
- target、target_ready、local_standalone_ready
- peer lease、child情報、Thunderbolt IP/discovery状態
- active standalone profile ID、model variant、residency

再接続の診断では、`generation`（`cluster_generation`と同値）と
`control_session.generation`を混同しない。後者はPairで再ネゴシエーションされるcontrol session
であり、`phase`、`lease.valid`、`lease.route-scoped`、`lease.peer-present`、peer descriptorの
generationと併せて確認する。`children` の `standalone` / `distributed-coordinator` /
`distributed-worker`について、PID、profile、generation、running、readyを現在のstateと照合する。

`POST /v1/pair` のHTTP 409（CLI/構造化logの `pair-generation-mismatch` を含む）は、古い
generation、idempotency conflict、phase不整合などのcontrol protocol拒否である。現行pair処理は
coordinatorが先に `/v1/node` を読み、双方のcontrol session generationの最大値をofferへ反映する。
反復する409は旧binary、persisted sessionの不一致、またはroute/lease failureを示す可能性がある
ため、`cluster status --json` と関連logの expected/received generationを保存し、state/model/cache
を削除して回避しない。

Human出力は次の形式を確認する。

```text
node=<node_id> role=<role> mode=<mode> state=<state> target=<target> ready=<bool>
```

Roleは`bridge0`のIPv4から決定する。`10.99.0.1`がcoordinator、`10.99.0.2`がworker、その他/未設定/競合はunknown。Role unknownではcluster listenerを開始しない。Roleはconfigで指定しない。

## 2. Doctor確認

`cluster doctor` は `/cluster` を取得し、次の3つのcheckを合成して `healthy` を返す。

- `target_ready`: 現在targetがreadyである。
- `safe_state`: stateが `booting` でも `manual-intervention-required` でもない。
- `admission_serving`: admission stateが `serving` である。

```sh
siderostat cluster doctor
siderostat cluster doctor --json
```

`healthy` は `target_ready && safe_state && admission_serving` の論理積である。`cluster doctor`はread-onlyで、状態を変更しない。

## 3. 構造化log

Log形式とlevelはconfigの `[logging]` で決める。

- `format = "json"`（既定）: tracing-subscriberのJSON形式。
- `format = "text"`: 平文形式。
- `level`: `siderostat=<level>` として適用する。既定は `info`。環境変数`RUST_LOG`が設定されていればそれを優先する。

Request完了ごとに `proxy_request` eventが出力される。path_templateは実pathを隠したtemplate（`/*` fallback）で、header/bodyをlogしない。Cluster transition、child restart、HELLO、deployment mismatch、peer discoveryは専用のmetrics/eventで記録される（spec第25.2節）。

Redaction（spec第25.3節）:

- Authorization/API key/Cookieをlogしない。
- request/response body、prompt、session/conversation IDをlogしない。
- peer proxy token、HMAC secret/signatureをlogしない。
- 完全model digest/deployment IDをlogしない。先頭12 hexのtagだけを診断に使える。

## 4. Metrics

`GET /metrics` はPrometheus text formatで返す。Loopback `127.0.0.1:18081`（既定）にのみbindする。

worker が Paired Standalone または Distributed (layer-parallel) で coordinator を target にしている
場合、メニューバーモニターは worker の `/metrics/coordinator` を利用する。この endpoint
は coordinator の loopback admin API を公開せず、worker の署名付き control request で
coordinator の `/v1/metrics` を取得する。worker が Solo Standalone の場合は従来どおり
worker 自身の `/metrics` を利用する。

```sh
curl --fail --silent http://127.0.0.1:18081/metrics
```

Spec第26節のfamilyを確認する。

- `ds4_proxy_requests_total{ingress,target,status_class}`
- `ds4_proxy_request_duration_seconds{target}`
- `ds4_proxy_time_to_first_byte_seconds{target}`
- `ds4_proxy_in_flight{ingress,target}`
- `ds4_proxy_target_ready{target}`
- `ds4_proxy_upstream_failures_total{target,reason}`
- `ds4_proxy_cluster_state{node_id,state}`
- `ds4_proxy_cluster_mode{node_id,mode}`
- `ds4_proxy_cluster_generation{node_id}`
- `ds4_proxy_cluster_peer_lease_seconds{node_id}`
- `ds4_proxy_thunderbolt_ip_state{node_id,state}`
- `ds4_proxy_peer_discovery_results{node_id,interface}`
- `ds4_proxy_peer_discovery_events_total{source,result}`
- `ds4_proxy_cluster_transitions_total{from,to,result,reason}`
- `ds4_proxy_cluster_transition_duration_seconds{transition}`
- `ds4_proxy_cluster_child_restarts_total{profile,reason}`
- `ds4_proxy_standalone_profile_info{node_id,quantization,speculative_support,residency}`
- `ds4_proxy_cluster_hello_total{result,reason}`
- `ds4_proxy_cluster_deployment_mismatch_total{field}`

`quantization`、`speculative_support`、`residency` は設定で許可した有限enumだけをlabel値にする。Profile ID、session、request ID、PID、generation、full digestをlabelにしない（spec第26節）。

## 5. Manual state

同一原因のpromotion失敗が3回連続したらauto promotionを止める。Standalone upstreamがreadyならproxyはServingを継続し、cluster stateだけ `ManualInterventionRequired` とする。Operator reconcileまで再試行しない（spec第18.6節）。

```sh
siderostat cluster status
siderostat cluster doctor
siderostat cluster reconcile
```

`cluster reconcile` はobserved stateをdesired stateへ収束させる。deployment mismatchのような自動修復できない不一致では、両nodeがSolo Standaloneへ収束し、自動pairingも停止する。原因を先に特定して取り除いてからreconcileを実行する。原因不明のままreconcileを繰り返しても、同一原因で再びSolo Standaloneへ収束する。

`cluster reconcile` はcoordinatorのpromotion failure trackerをresetし、`ManualInterventionRequired` からの解除とを一つのatomicな操作として扱う（plan B-03）。そのため、原因除去後のreconcileでtrackerの失敗回数が持ち越されず、次のpromotion試行は失敗回数0から再開する。deployment mismatchからの復旧後は `siderostat cluster status` で `state` が `solo-standalone-ready` から期待するpaired stateへ進むことを確認する。原因不明のままreconcileを繰り返しても、同一原因で再びSolo Standaloneへ収束する。

PeerLost recovery中の `cluster reconcile` は、まず `PeerLossRecovery` ownerによる local recovery
（admission block → distributed child stop → standalone start → SoloStandaloneReady）へ収束する。
Backoff中もpeer lossがbackoff deadlineより優先される。`cluster reconcile` はpair 409を強制的に
無視したりstate fileを初期化したりする操作ではない。

## 6. Safe restart

`cluster restart` はcurrent profileのchildを再起動する。Modeを変えず、admission/drainに連動する。

```sh
siderostat cluster restart
```

- 通常停止はSIGTERM。Drain完了後にsignalする（spec第20.3節）。
- Stop timeout後のSIGKILLは既定で許可されるが、identityを直前に再確認できたowned childだけを対象とする。`allow_sigkill=false` に変更した場合はmanual intervention（spec第12.4節）。
- Unknown processへ無条件にsignalしない。起動時に既存の `siderostat` / `ds4-server` を検出した場合は、macOS右上の簡潔な通知と警告音を出し、既定では5秒後に各signal直前のidentity再確認後にSIGTERM、必要ならSIGKILLを送る。拒否は `startup_cleanup.auto_restart = false` または `--decline-startup-cleanup` で指定する。拒否・identity不一致・停止失敗時は新しいsiderostatを起動しない（spec第20.2節、第38節）。
- Restartはmodeを変えない。Paired Standalone / Distributed (layer-parallel) 中のchild再起動はmode遷移を引き起こさない。

## 7. Rollback

Rollbackは [`docs/installation.md`](installation.md) のRollback節に従う。

- Legacy config v1の `backends`、`routing`、`affinity`、`heartbeat`、`active_probe`、`cooldown`、SQLite pathは廃止されている。Unknown/legacy fieldを黙って無視しない（spec第22.4節）。
- 旧affinity databaseを自動削除しない。読まない、変更しない、削除しない（spec第22.4節、第38節）。
- 新configは旧configと分離し、`schema_version == 2` の作業fileを保持する。
- Binary rollbackは直前のbinaryを残し、standalone readinessを確認してから行う。Upgrade後にrollbackし、再度upgradeする（plan P7-02）。
- 起動前に `cluster doctor` と `/readyz` でstandalone readinessを確認する。Rollback後はSolo Standalone readyを確認してからpairing/promotionへ進む。
- candidateを継続利用する判定でも、rollback binary/config/state/plistを緊急復旧用に保持する。正規
  名称のRelease Candidate artifactとchecksumの確定は、merge後の再生成・再検証で行う。

## 8. 安全な運用原則

- Destructive cache削除を通常手順にしない。KV cache、state file、secretの削除は自動で行わず、operatorが明示的に判断する場合だけ実施する。
- Secret/token fileは32 bytes以上の生バイト列、mode `0600`、相互に異なるpathで配置する。SSH秘密鍵やPEMではなく、同じfileを複数roleに流用しない。
- Admin mutationはloopbackでもtoken必須。GETはsecretを返さない。
- Mode切替は503の短いwindowを含む。切替中に新規requestは503 + `Retry-After`で拒否され、既存streamは完走する。
- Workerからcoordinatorへのrequestはproxyを2 hop通る。Peer data/DS4 native trafficは暗号化されない。専用の物理Thunderbolt linkを信頼境界とする。
