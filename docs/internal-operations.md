# siderostat 運用ガイド

この文書は、`docs/spec.md`と実装済みのadmin API / CLI / logging / metricsに基づき、siderostatの運用手順を定める。実DS4 binaryとmodelを使う導入は [`docs/internal-installation.md`](internal-installation.md) を参照する。

## 0. 通常のインストールと起動

エンドユーザーの install／upgrade は、署名・公証済み DMG の `.pkg` を Finder から実行する。
package は既存の product-owned runtime と Siderostat Monitor を置換前に停止し、成功後に
`/Applications/Siderostat.app` を起動する。手動の `launchctl bootstrap`、`killall`、別の
`ds4-server` LaunchAgent は作成しない。

Login Items／Background Items の承認は Installer の管理者認証とは別であり、必要な場合は
System Settings > General > Login Items で承認する。アンインストールは DMG 内の
`Siderostat Uninstaller.app` を Finder から起動する。設定、secret、manifest、cluster state、model、
KV cache は保持される。

標準の runtime background service は bundle 内 plist
`dev.siderostat-ds4-proxy.runtime.plist` と `SMAppService` で管理される。以下の `launchctl` 操作は
開発・診断時だけ使用し、配布版の通常操作には使用しない。

## SSH での診断コマンド

Mac Studio などへ SSH した非対話 shell では、Homebrew の `/opt/homebrew/bin` が `PATH` に
含まれないことがある。`rg` がインストール済みでも `command not found` になる場合は、絶対
path を使うか、診断コマンドの先頭で PATH を補う。

`REMOTE_USER`、`COORDINATOR_HOST`、`<coordinator-bridge-ip>`、`<worker-bridge-ip>` は、各利用環境の値へ置き換えるプレースホルダーであり、リポジトリへ実値を記録しない。

```sh
ssh "${REMOTE_USER}@${COORDINATOR_HOST}" 'PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin; export PATH; command -v rg; rg --version'
```

これは `rg` のインストール状態の問題ではなく、SSH session の環境差である。agent が read-only
証跡を収集するときも同じ PATH を明示する。

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

Roleは`bridge0`のIPv4から決定する。`<coordinator-bridge-ip>`がcoordinator、`<worker-bridge-ip>`がworker、その他/未設定/競合はunknown。Role unknownではcluster listenerを開始しない。Roleはconfigで指定しない。

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

## 2.1 Bounded canary

実推論の応答速度を確認する場合は、次の read-only CLI を coordinator または対象 node
で実行する。

```sh
siderostat cluster canary
siderostat cluster canary --json
```

canary は設定済みの local public endpoint の `POST /v1/chat/completions` に一回だけ送信する。
prompt は `Reply with the single word: OK.`、`max_tokens` は64、`stream` は有効で固定されており、
任意 URL、任意 prompt、任意 token 数、外部課金先を指定する引数はない。出力には elapsed、TTFB、
生成 token 数、chunk TPS、HTTP status、有限の reason code だけを含め、prompt・response body・
Authorization・API key・session ID・request ID は含めない。

`reason=healthy` 以外ではコマンドは失敗終了する。`deadline` は first-token を含む deadline 超過、
`http_error` は HTTP またはストリーム形式のエラー、`low_decode_tps` は継続 token の低速、
`progress_stall` はストリーム中の進捗停止を示す。canary 自体は cluster state、admission、child
lifecycle を変更しない。

## 2.2 Degraded recovery job

`throughput-degraded` の復旧要求は coordinator だけが作成できる。admin token と、検出理由に対応する
trigger を明示する。

```sh
siderostat cluster recover-degraded \
  --reason throughput-degraded \
  --trigger manual-canary-failure \
  --json
siderostat cluster recover-degraded --status <recovery-id> --json
```

recovery job は単一 owner、冪等性、bounded history、認証・role/state gate と、cluster mutation 前の
diagnostic snapshot を経てから既存 lifecycle owner の demote/promote を一回だけ実行する。recovery
専用の admission drain timeout は60秒で、通常 lifecycle の timeout とは分離されている。再構成中は
外部 request を block したまま、DistributedReady 後に30秒・一回限りの内部 canary permitを発行する。
canary が `healthy` の場合だけ admission を `serving` に戻し、そうでなければ block を維持して
`failed` とする。gate 拒否、snapshot 失敗、drain timeout、lifecycle failure、stale recovery ID は
自動 retry や worker-only restart を行わない。status 応答の snapshot path はローカルのユーザー名や
絶対パスを含まない recovery-scoped identifier である。

自動検知は設定の [recovery] で opt-in する。既定値は enabled = false であり、無効時は
進捗を観測して metrics に記録するだけで、recovery job は自動開始しない。有効化した場合も、
decode TPS が30秒継続して5 tokens/s未満、progress ageが60秒以上、または canary failure の
いずれか一事象につき一回だけ既存の recovery ownerへ送る。cooldownは1時間、12時間内の上限は
2回で、gate拒否・失敗後の自動 retry は行わない。H-10の実機 change window 外で
enabled = true にしてはならない。

## 2.3 Hermes cron 前の canary handoff

Hermes の cron を開始する前に、同じ node の loopback endpoint で次を順番に実行する。

```sh
siderostat cluster doctor --json
siderostat cluster canary --json
```

`doctor.healthy=true` かつ canary の `status=healthy` / HTTP 200 を確認できた場合だけ
cron を開始する。canary が失敗した場合は cron を開始せず、`cluster status --json` と
実行中の recovery status を確認して operator の判断を待つ。cron の retry や stale
deadline を recovery owner の代替にしない。

暫定 deadline の順序は Hermes stale timeout `1800s`、Siderostat の
`first_body_byte` / `stream_idle` `2400s` とする。これは request の期限であり、canary の
TTFB/chunk TPS 判定値ではない。canary は cron 前の serving gate として一回だけ実行し、
任意 prompt や任意 endpoint の probe には使用しない。

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
- `ds4_proxy_ds4_prefill_last_progress_age_seconds`
- `ds4_proxy_ds4_prefill_progress_token_delta`
- `ds4_proxy_ds4_generation_progress_observed`
- `ds4_proxy_ds4_generation_last_progress_age_seconds`
- `ds4_proxy_ds4_generation_progress_token_delta`

`quantization`、`speculative_support`、`residency` は設定で許可した有限enumだけをlabel値にする。Profile ID、session、request ID、PID、generation、full digestをlabelにしない（spec第26節）。

`ds4_proxy_ds4_generation_active=1` かつ `ds4_proxy_ds4_generation_progress_observed=0` の場合は、
最初の DS4 progress event をまだ受信していない first-token waiting である。active 中の
`*_last_progress_age_seconds` は monotonic clock に基づく受信経過時間であり、idle/完了時は `0` になる。

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

Rollbackは [`docs/internal-installation.md`](internal-installation.md) のRollback節に従う。

- Legacy config v1の `backends`、`routing`、`affinity`、`heartbeat`、`active_probe`、`cooldown`、SQLite pathは廃止されている。Unknown/legacy fieldを黙って無視しない（spec第22.4節）。
- 旧affinity databaseを自動削除しない。読まない、変更しない、削除しない（spec第22.4節、第38節）。
- 新configは旧configと分離し、`schema_version == 2` の作業fileを保持する。
- Binary rollbackは直前のbinaryを残し、standalone readinessを確認してから行う。Upgrade後にrollbackし、再度upgradeする（plan P7-02）。
- macOS app bundle の downgrade は通常の `.pkg` では行わず、配布側で `cargo xtask sign --rollback`
  により生成した `Siderostat-<version>-rollback.pkg` を明示的に指定する。通常 package は bundle
  version check を維持する。installer は bundle replacement 前に active console user の
  product-owned runtime LaunchAgent を一時 unload し、Monitor を exact path で停止するが、
  LaunchAgent の恒久登録、設定、secret、model、cache は変更しない。
- 起動前に `cluster doctor` と `/readyz` でstandalone readinessを確認する。Rollback後はSolo Standalone readyを確認してからpairing/promotionへ進む。
- candidateを継続利用する判定でも、rollback binary/config/state/plistを緊急復旧用に保持する。正規
  名称のRelease Candidate artifactとchecksumの確定は、merge後の再生成・再検証で行う。
- rollback package のインストール後は、両 node の `healthz`／`readyz`、`cluster doctor`、
  `state=solo-standalone-ready` を確認してから pairing／promotionへ進む。

## 8. 安全な運用原則

- Destructive cache削除を通常手順にしない。KV cache、state file、secretの削除は自動で行わず、operatorが明示的に判断する場合だけ実施する。
- Secret/token fileは32 bytes以上の生バイト列、mode `0600`、相互に異なるpathで配置する。SSH秘密鍵やPEMではなく、同じfileを複数roleに流用しない。
- Admin mutationはloopbackでもtoken必須。GETはsecretを返さない。
- Mode切替は503の短いwindowを含む。切替中に新規requestは503 + `Retry-After`で拒否され、既存streamは完走する。
- Workerからcoordinatorへのrequestはproxyを2 hop通る。Peer data/DS4 native trafficは暗号化されない。専用の物理Thunderbolt linkを信頼境界とする。

## 9. DMG Uninstaller

リリース DMG は `Siderostat-<version>.pkg`、`Siderostat Uninstaller.app`、`README.html` の3項目だけを
収録する。エンドユーザーは `Siderostat Uninstaller.app` を Finder から起動し、確認 UI の承認後に
アンインストールを実行する。

Uninstaller は runtime と Siderostat のログイン項目を `SMAppService` で解除し、固定 executable path
と PID identity が一致するプロセスだけを停止してから `/Applications/Siderostat.app` を Trash へ移動する。
設定、secret、manifest、cluster state、model、KV cacheは保持する。処理が一部完了した状態でも再実行
でき、Terminal の `sudo rm -rf` や `killall` は使用しない。
