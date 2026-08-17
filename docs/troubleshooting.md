# siderostat troubleshooting

この文書は、`docs/spec.md`と実装済みのadmin API / CLI / logging / metricsに基づき、failure symptom別に該当手順を定める。運用の基本は [`docs/operations.md`](operations.md) を参照する。実DS4/modelを使う検証は [`docs/installation.md`](installation.md) に従う。

## 1. 診断の基本手順

症状が出たら、次の順で現状を把握する。いずれもread-onlyで状態を変更しない。

```sh
siderostat cluster status
siderostat cluster doctor
curl --fail --silent http://127.0.0.1:18081/readyz
curl --fail --silent http://127.0.0.1:18081/cluster
curl --fail --silent http://127.0.0.1:18081/metrics
```

`cluster doctor`の `healthy` は `target_ready && safe_state && admission_serving` の論理積である。`safe_state` がfalseのときは `booting` または `manual-intervention-required` を疑う。

構造化logの `proxy_request` event、cluster event、metricsの `ds4_proxy_*` familyで遷移原因を確認する。Header/body、prompt、session/conversation ID、secret、完全digestはlogされない（spec第25.3節）。

## 2. Failure symptom別手順

| 症状 | 確認 | 手順 |
|---|---|---|
| Peer不在で両nodeがSolo Standalone | `cluster status`のmode/state、peer lease | そのままlocal standaloneを運用。Peer present条件を満たしたら自動でpairingへ進む |
| `bridge0`が存在しない/UPでない | doctor、Thunderbolt IP state | System SettingsでThunderbolt Bridge serviceを有効にする。Proxyは設定を変更しない |
| `bridge0`のaddressが未設定/競合 | role unknown、cluster listener停止 | 固定IPv4を設定し直す。Role unknownではlocal standaloneを維持 |
| Bonjourが利用不能 | discovery mode、static fallback | `bonjour-with-static-fallback`なら設定済みcounterpart addressへfallbackする |
| Bonjour発見だけではpairingしない | peer lease、HMAC handshake | 発見結果だけをtrustせず、`bridge0` route、HMAC、leaseを確認する |
| Control HMACが不一致/clock skew/nonce replay | control log、metrics | Secret fileとclock skewを確認する。不正HMACは拒否され、stateは進まない |
| PairがHTTP 409で反復 | `cluster status --json`のcluster/control generation、control phase/lease、pair log | 現行coordinatorのoffer/confirmはpeerのgenerationを先に取り込む。旧binary、persisted session不一致、route/lease失敗を確認し、state/model/cacheを削除せず証跡を保存する |
| Peer ingressがwrong source/token/hopを拒否 | peer ingress log | Peer proxy tokenとsource IPを確認する。専用の物理Thunderbolt linkを信頼境界とする |
| Deployment mismatch | `cluster status`、deployment_mismatch_total | Paired Standaloneを維持し、promotionは拒否される。Binary/model/checkpoint/argvを一致させる |
| Manifest stale | fingerprint job | `cluster fingerprint` で再fingerprintする。staleの間はpromotionしない |
| Hello timeout | rendezvous、hello_total | 実DS4 worker HELLOなしでpromotionしない。Rendezvous timeout後に原因を確認する |
| Unknown DS4 schema | log、hello_total | fail closedでpromotionを拒否する。Unknown formatはlog転送だけ行いstateを進めない |
| Coordinator startup timeout | coordinator startup log | `--debug`のcoordinator起動を確認する。Startup timeout後はstandaloneへ収束する |
| Route incomplete | complete route log | Complete route前にadmission再開しない。Layer splitのgap/overlapを確認する |
| Peer lease喪失 | peer_lease_seconds | Lease失効でfuture admissionを閉じ、Solo Standaloneへ収束する |
| Unknown processが必要portを占有 | doctor、child identity | macOS右上の簡潔な通知と警告音を出し、既定5秒後にidentity再確認後に終了する。`startup_cleanup.auto_restart = false` または `--decline-startup-cleanup` で拒否し、ManualInterventionRequired相当として対応する |
| Standalone startup failed | standalone startup log | Standalone profileの起動を確認する。Startup失敗時はUnavailableとして503を返す |
| Drain timeout | drain log、in_flight | 既定ではidentity確認済みowned childだけSIGKILLできる。`allow_sigkill=false`ならmanual intervention |
| State corrupt | state log、doctor | Corrupt stateを保全し、standalone_safeならSolo Standalone、不能ならManualInterventionRequired |
| Promotion失敗が3回連続 | cluster state、backoff | Auto promotionを止め、原因を取り除いてから`cluster reconcile`を実行する |

## 3. 症状別の詳細手順

### 3.1 Peer不在 / Solo Standalone

Peer presentは次をすべて満たす状態だけである（spec第9.3節、第13.1節）。

- `bridge0` に期待するlocal addressがある。
- Bonjourまたはstatic fallbackで得たremote IPが期待peer addressで、`bridge0` scoped routeを持つ。
- HMAC認証済みnode descriptorを受信できる。
- control leaseが有効である。
- `required_peer_stability`（既定5秒）の間、条件が継続する。

ICMP echoだけでpeer presentと判定しない。Bonjour結果だけではpairingしない。Peer不在では両nodeが自身のstandalone profileを提供するSolo Standaloneを維持する。`cluster doctor`で `healthy=true` を確認する。

### 3.2 Thunderbolt / role

Proxyはnetwork設定を作成、有効化、address変更しない（spec第13.1節）。`Thunderbolt Bridge`という表示名だけでは識別しない。`bridge0`のIPv4が `10.99.0.1`（coordinator）か `10.99.0.2`（worker）でなければrole unknownとし、cluster listener/peer ingress/distributedを開始しない。Local standaloneがreadyならpublic ingressはLocalStandaloneへ転送する。System Settingsで固定IPv4を設定し直し、doctorでThunderbolt IP stateが期待状態になることを確認する。Apple公式手順（spec第39節）を参照する。

### 3.3 Peer discovery / authentication

Bonjour advertisement/browseは`bridge0`に限定される。Resolved addressが期待subnet上にあり、local addressではなく、routeが`bridge0` scopedであることを確認してpeer candidateとする。その後、HMAC control handshakeでnode descriptor、role、protocol versionを検証する。Bonjour発見だけではpeer presentにしない。Control planeはHMAC、timestamp、nonce、source IPを検証する（spec第13.3節、第27節）。通常LAN経由でpeer controlを確立してはならない。

### 3.4 Deployment mismatch / promotion拒否

Distributed profileの承認済みbinary digest集合、full source commit、model digest、checkpoint、context、layer split、wire schema、argv profileを比較する。各nodeのlocal binary digestが共通の承認集合に含まれない場合、またはcompatibility fieldが1つでも不一致/不明ならMXFP4 promotionを拒否するが、Paired Standaloneは維持できる（spec第15.3節）。`ds4_proxy_cluster_deployment_mismatch_total{field}`で不一致fieldを確認する。未知binaryを集合へ自動追加せず、対象digest pairのactual acceptanceをcompatibility recordへ記録してから両manifestの集合を同時更新する。両nodeのMXFP4 content SHA-256を一致させ、manifestを再fingerprintしてから再試行する。実HELLOなしでpromotionしない（spec第33.3節）。

### 3.5 Hello / route

Coordinatorが `10.99.0.1:9911` でrendezvous listenerを開始し、実DS4 worker HELLOを受信してからpromotionする。Complete route前にadmission再開しない。HTTP listeningだけでDistributedReadyにしない。Rendezvous HELLO timeout（既定900秒）を超えた場合は、workerの起動・lease・deploymentを確認してから再試行する。Unknown DS4 wire/log formatではstateを進めず、log転送だけ行う。

### 3.6 Peer lease喪失 / route loss

Peer controlは生きているがrouteが失われた場合は、route loss grace（既定15秒）経過後Paired Standaloneへ復帰する（spec第18.4節）。Peer lease/linkが失われた場合はSolo Standaloneへ収束する（spec第18.5節）。Lease失効でfuture admissionを閉じる。`ds4_proxy_cluster_peer_lease_seconds`でleaseを確認し、cable再接続後はdebounceしたpeer discoveryが自動で再評価される。

### 3.7 Child supervision / restart

DS4 childをTokio process APIでspawnし、shellを介さない。PID、canonical executable、argv hash、profile、generation、spawn timestampを保存する。Proxy再起動時はexecutable、argv、start timeを照合し、完全一致したowned childだけを自動で再所有または停止対象にする。起動時に既存の `siderostat` / `ds4-server` が見つかった場合は、macOS右上の簡潔な通知と警告音を出し、既定では5秒後に各signal直前のidentity再確認後にSIGTERM、必要ならSIGKILLを送る。拒否は `startup_cleanup.auto_restart = false` または `--decline-startup-cleanup` で指定する。拒否、identity不一致、停止失敗の場合は起動せずManualIntervention相当で止まる。通常停止のstop timeout後SIGKILLも、既定でidentity確認済みowned childだけを対象にする。`allow_sigkill=false` ならmanual intervention。

### 3.8 Standalone startup / drain timeout

Standalone startup timeout（既定900秒）を超えた場合、standalone profileの起動を確認する。Standalone upstreamがreadyでなければproxyはUnavailableとして503 + `Retry-After`を返す。Drain timeoutを超えた場合、進行中requestを別modelで再実行しない。既定ではidentity確認済みowned childだけをSIGKILLでき、`allow_sigkill=false` ならmanual intervention（spec第12.4節）。

### 3.9 Promotion backoff / manual state

同一原因のpromotion失敗が3回連続したらauto promotionを止める。Standalone upstreamがreadyならproxyはServingを継続し、cluster stateだけ `ManualInterventionRequired` とする。Operator reconcileまで再試行しない（spec第18.6節）。原因を取り除いてから `cluster reconcile` を実行する。`cluster reconcile` はcoordinatorのpromotion failure trackerをresetし、`ManualInterventionRequired` からの解除とを一つのatomicな操作として扱うため、解除後は同一原因の失敗回数が持ち越されず、次のpromotion試行は失敗回数0から再開する（plan B-03）。解除後に `siderostat cluster status` でstable stateへ戻り、trackerの失敗回数が保持されないことを確認する。原因不明のままreconcileを繰り返しても、同一原因で再びbackoffへ入る。

### 3.10 Pair 409 / control session

`POST /v1/pair` の409は、`GenerationMismatch`、idempotency conflict、peer phase不整合などの
control protocol拒否である。`cluster status --json`で次を同時に保存する。

- `cluster_generation`（state transitionの世代）。
- `control_session.generation`、`phase`、`lease.valid`、`lease.route-scoped`、`lease.peer-present`。
- `children` の各 child identityとrunning/ready。

現行のcoordinatorは `/v1/node` のpeer session generationを取得し、双方の既知値の最大値をPair
offerへ反映する。したがって同じ409が周期的に続く場合は、旧candidate binaryの残存、永続sessionの
不一致、route-scoped evidenceの失効、HMAC/source検証失敗を優先して確認する。`cluster reconcile`
はpromotion failure trackerのresetであり、Pair 409を無条件に解消する操作ではない。state file、
model、KV cacheを削除せず、expected/received generationを含むredacted logを保存する。

### 3.11 State corrupt

State fileはtemp write、file sync、atomic renameで保全する。Secret/tokenを保存しない。Single-instance file lockを取得する。Corrupt stateを保全し、安全確認可能ならSolo Standalone、不能ならManualInterventionRequired（spec第24節）。旧SQLite affinity databaseを読まない、変更しない、削除しない。

## 4. 運用上の注意

- Destructive cache削除を通常手順にしない。KV cache、state file、secretの削除は自動で行わず、operatorが明示的に判断する場合だけ実施する。
- Mode切替は503の短いwindowを含む。切替中に新規requestは503 + `Retry-After`で拒否され、既存streamは完走する。Requestを二重実行しない（spec第5節）。
- Workerからcoordinatorへのrequestはproxyを2 hop通る。Peer data/DS4 native trafficは暗号化されない。専用の物理Thunderbolt linkを信頼境界とする。
- v0.1.0の実DS4 baselineはfull commit `b0309611041655f4e45671cfd9c9886aff161406`である。Production enable前に `docs/compatibility/ds4-b030961.md` のapproved native binary集合とModel/profile matrixに一致することを確認する。
