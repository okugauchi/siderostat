# DS4 Mode-Aware Reverse Proxy / Cluster Supervisor

## 1. 文書情報

| 項目 | 値 |
|---|---|
| 文書状態 | Target Specification / 実装着手可能 |
| 最終更新日 | 2026-08-06 |
| 対象platform | macOS 26系、Apple Silicon |
| 初期topology | Thunderbolt Bridgeで直結した2 node |
| DS4互換性基準 | full commit `b0309611041655f4e45671cfd9c9886aff161406` |
| 実装言語 | Rust stable、edition 2024 |

本書の `MUST`、`MUST NOT`、`SHOULD`、`SHOULD NOT`、`MAY` は、それぞれ必須、禁止、推奨、非推奨、任意を表す。実装者は明示的な仕様変更なしに `MUST` と `MUST NOT` を変更してはならない。

本書だけでtarget behavior、protocol、configuration、state transition、test、完了条件を把握できることを目的とする。

## 2. 目的

`siderostat` は、固定されたOpenAI互換HTTP endpointを提供し、cluster modeに応じて転送先を一意に切り替えるmode-aware reverse proxyである。同じbinaryがDS4 child processと2 node clusterのlifecycleも管理する。

期待する基本動作：

```text
peer不在
  -> 各nodeのproxyは、そのnodeで設定されたstandalone profileへ転送

peer接続済み、MXFP4 distributed未完成または不適合
  -> coordinator proxyはlocal standalone profileへ転送
  -> worker proxyはcoordinatorのpeer ingressへ転送
  -> workerのlocal standalone profileは停止可能

MXFP4 distributed ready
  -> coordinator proxyはlocal MXFP4 coordinatorへ転送
  -> worker proxyはcoordinatorのpeer ingressへ転送

peerまたはdistributed route喪失
  -> coordinatorは設定済みstandalone profileへ復帰
  -> peerが残っていればworkerはcoordinator standaloneへ転送
  -> peerが不在ならworkerは自身のstandalone profileを起動して転送
```

転送先の選択にpriority、least-busy、EWMA、session affinityなどを使用しない。同じmodeでは転送先が常に1つに決まる。

## 3. Goals

- OpenAI互換request/responseを透過的にstreaming中継する。
- 公開proxy processとlisten portをDS4 mode切替中も維持する。
- peer不在時はlocal standalone profileへ転送する。
- peer存在時はcoordinatorへ転送を集約する。
- standalone profileとしてQ2、Q2-Q4、MXFP4を選択でき、常駐ロードまたはDS4のSSD streamingを利用できる。
- compatibleなpeerが揃った場合、standalone profileからMXFP4 distributedへ自動昇格する。
- distributed昇格を実DS4 worker HELLOとcomplete routeで確認する。
- worker/route喪失時にstandalone profileへ自動降格する。
- standalone deploymentとdistributed deploymentのKV cacheを分離する。
- proxy admissionとDS4 process drainを連動する。
- DS4 source/protocol contract、承認済みbinary集合、model、checkpoint、argvの互換性をfail closedで検証する。
- coordinator/worker両nodeで、実機acceptance済みの同一binary compatibility集合を利用する。
- LaunchAgent、local CLI、構造化log、metrics、admin APIを提供する。
- 外部databaseまたはcluster state serviceなしで動作する。
- 実装順序、migration、verification、rollback、README全面刷新、およびDS4本体のセットアップを含む導入ガイド作成を扱う実装計画を、仕様書から独立した文書として維持する。

## 4. Non-goals

- 複数backend間のload balancing。
- priority、least-in-flight、EWMA latencyによるbackend選択。
- session/prefix affinityによるbackend選択。
- request単位のalternate backend retry。
- backend cooldown/circuit breakerによる選択制御。
- DS4 distributed inference自体の再実装。
- GPU scheduling、request batching、tokenization。
- 3 node以上の任意topology。
- standalone deploymentとdistributed deploymentのlive KV state変換。
- streaming開始後のrequest移送または再実行。
- OAuth/OIDC、課金、multi-tenant quota。
- Kubernetes、Consul、etcdなどの外部orchestrator。
- 通常LANまたはInternet上のcluster形成。

## 5. 設計原則

1. **転送先はmodeだけで一意に決める。** 負荷やsession IDで変更しない。
2. **proxy processをmode切替のために停止しない。** DS4 childだけを切り替える。
3. **coordinatorがDS4 HTTP requestを一元的に観測する。** workerはcoordinatorの専用peer ingressへ転送する。
4. **DS4切替前に新規admissionを閉じる。** in-flight streamが0になってからSIGTERMする。
5. **実HELLOなしでdistributedへ昇格しない。** process存在やnetwork到達性だけを信用しない。
6. **Distributed deploymentをfail closedで保証する。** 不明、欠損、不一致ではstandalone serviceを維持する。
7. **StandaloneとdistributedのKVを共有しない。** 同じGGUFを使う場合もnamespaceを分け、mode切替後はtranscriptから再構築する。
8. **requestを二重実行しない。** mode変更による自動retryを行わない。
9. **unknown processをkillしない。** PID以外のidentityを検証する。
10. **自動復旧を有限時間・有限回数にする。** restart loopを避ける。
11. **unknown DS4 wire/log変更を推測しない。** promotionを拒否する。
12. **秘密値、prompt、session identifierをlogへ出さない。**

## 6. 用語

| 用語 | 定義 |
|---|---|
| Public ingress | clientが接続するloopback HTTP listener |
| Peer ingress | worker proxyからのrequestだけを受けるcoordinatorのThunderbolt listener |
| Local upstream | 同一nodeで管理されるDS4 HTTP endpoint |
| Standalone profile | 単一nodeで起動するmodel、量子化、residency、context、KV、追加引数の設定集合 |
| Residency | 通常どおりmodelをmapする `resident`、またはDS4の `--ssd-streaming` を使う `ssd-streaming` |
| Proxy target | `LocalStandalone`、`Coordinator`、`Unavailable` のいずれか |
| Coordinator node | `bridge0=10.99.0.1` のnode |
| Worker node | `bridge0=10.99.0.2` のnode |
| Solo Standalone | peer不在で各nodeが自身のstandalone profileを提供するmode |
| Paired Standalone | peer存在時にrequestをcoordinatorのstandalone profileへ集約するmode |
| Distributed MXFP4 | coordinator/workerでMXFP4をpipeline parallel実行するmode |
| Admission gate | 新規proxy requestを受理するかをtarget単位で直列化するgate |
| In-flight | upstream response bodyのEOF、error、client切断までpermitを保持するrequest |
| Deployment | DS4 binary、model、checkpoint、context、residency、argv、およびdistributed時のlayer splitを含む実行単位 |
| Deployment ID | canonical deployment manifestのSHA-256 |
| Generation | cluster transitionを識別する単調増加整数 |
| Control lease | peer processがgenerationへ参加中であることを示す期限付きmembership |
| Rendezvous listener | 実DS4 worker HELLOを捕捉するcoordinator上の一時listener |
| Complete route | coordinatorとworkerのlayerがgapなくmodel全体を覆う状態 |
| Drain | 新規admissionを閉じ、既存in-flightが0になるのを待つ処理 |

## 7. Topology

```text
Client on coordinator node
  -> Public ingress 127.0.0.1:18080
       -> Local upstream 127.0.0.1:8000

Client on worker node
  -> Public ingress 127.0.0.1:18080
       |
       | Solo Standalone
       +-> Local upstream 127.0.0.1:8000
       |
       | Paired Standalone / Distributed MXFP4
       `-> Coordinator peer ingress 10.99.0.1:18082
             -> Coordinator local upstream 127.0.0.1:8000

Cluster control
  coordinator 10.99.0.1:9920 <-> worker 10.99.0.2:9920

DS4 distributed native protocol
  coordinator 10.99.0.1:9911 <- worker
```

DS4 HTTP endpointは両nodeともloopbackにbindする。Worker requestはcoordinatorのpeer ingressを経由するため、DS4 HTTP portをThunderbolt Bridgeまたは通常LANへ公開しない。

## 8. Process architecture

```text
siderostat process
  +-- Public ingress
  +-- Coordinator-only peer ingress
  +-- Streaming forwarder
  +-- Mode-aware target resolver
  +-- Admission gate / in-flight tracker
  +-- Admin API / CLI server
  +-- Cluster state machine
  +-- Peer control client/server
  +-- Deployment verifier
  +-- DS4 HELLO rendezvous listener
  +-- DS4 child supervisor
  +-- DS4 log parser
  +-- Persistent state store
  +-- Metrics / structured logging
       |
       `-- owned ds4-server child
```

別のcluster binaryまたはdaemonを作らない。Proxy、cluster state machine、DS4 process supervisorは同じprocess内で動作する。

## 9. Modeと転送規則

### 9.1 Cluster mode

```rust
enum StableMode {
    SoloStandalone,
    PairedStandalone,
    DistributedMxfp4,
}

enum ClusterState {
    Booting,
    SoloStandaloneStarting,
    SoloStandaloneReady,
    Pairing,
    PairedStandaloneReady,
    AwaitingWorkerHello,
    Promoting,
    DistributedStarting,
    DistributedReady,
    Demoting,
    Backoff,
    ManualInterventionRequired,
}
```

### 9.2 Proxy target

```rust
enum ProxyTarget {
    LocalStandalone,
    Coordinator,
    Unavailable { reason: UnavailableReason },
}
```

Target resolutionは次の表だけで決める。

| local role | stable mode/state | target |
|---|---|---|
| coordinator | Solo Standalone ready | local upstream |
| worker | Solo Standalone ready | local upstream |
| coordinator | Paired Standalone ready | local upstream |
| worker | Paired Standalone ready | coordinator peer ingress |
| coordinator | Distributed ready | local upstream |
| worker | Distributed ready | coordinator peer ingress |
| any | target drain中、upstream starting/stopped | unavailable |
| unknown role | local standalone ready | local upstream |
| unknown role | local standaloneなし | unavailable |

同じstateでtarget候補を複数生成してはならない。Request body、header、model field、session、負荷、latencyによってtargetを変更してはならない。

### 9.3 Peer presenceの意味

Peer presentは次をすべて満たす状態とする。

- `bridge0` に期待するlocal addressがある。
- Bonjourまたはstatic fallbackで得たremote IPが期待peer addressで、`bridge0` scoped routeを持つ。
- HMAC認証済みnode descriptorを受信できる。
- control leaseが有効である。
- `required_peer_stability` の間、条件が継続する。

ICMP echoだけでpeer presentと判定しない。

Peer presentになったら、まずPaired Standaloneを形成する。MXFP4 deploymentが一致すれば、その後Distributed MXFP4へのpromotionを試みる。自動修復できないdeployment不一致を検出した場合は、両nodeをSolo Standaloneへ収束させ、同じ原因による自動pairingの再試行を止める。

## 10. HTTP reverse proxy contract

### 10.1 Public ingress

最低限次を透過中継する。

```text
GET  /v1/models
POST /v1/chat/completions
POST /v1/responses
POST /v1/completions
POST /v1/messages
```

予約したadmin path以外のunknown pathも同じtargetへ転送する。

### 10.2 Method、URI、body

- HTTP methodを保持する。
- pathとquery stringを保持する。
- Request bodyを変更しない。
- Body全体をbufferせずstreaming転送する。
- Mode判定のためbodyをparseしない。
- Body size上限をstream中の累積byte数でも検証し、超過時はupstream streamをabortする。
- `Content-Type` を保持する。
- Content length/chunked semanticsはHTTP stackに正しく再構成させる。

### 10.3 Header

Hop-by-hop headerを転送しない。

```text
Connection
Keep-Alive
Proxy-Authenticate
Proxy-Authorization
TE
Trailer
Transfer-Encoding
Upgrade
```

End-to-end headerと `Authorization` を転送するがlogへ出さない。次を設定する。

```text
X-Forwarded-For
X-Forwarded-Host
X-Forwarded-Proto
X-Request-Id
Via
```

Public clientが送った `X-DS4-Peer-Proxy-Token`、`X-DS4-Proxy-Hop`、`X-DS4-Cluster-*` は必ず除去し、内部値だけを必要なhopで生成する。

### 10.4 Response

- Status codeとend-to-end headerを保持する。
- Response bodyをbufferせずstreamingする。
- Chunk順序とbackpressureを保持する。
- Client disconnectをupstream cancellationへ伝播する。
- Response bodyのEOF、error、dropまでin-flight permitを保持する。

### 10.5 Retry

Proxyはalternate targetへのretryを行わない。Connect failure、timeout、5xx、mid-stream failureを受けたrequestは、そのrequestのtargetで終了する。

Cluster subsystemはfailureを観測してfuture request用modeを変更できるが、失敗済みrequestをstandaloneまたはdistributedで自動再実行しない。Clientが同一requestを再送するか判断する。

### 10.6 Error response

Proxy自身のerrorはOpenAI風JSONで返す。

```json
{
  "error": {
    "message": "DS4 target is temporarily unavailable during a mode transition",
    "type": "service_unavailable",
    "code": "mode_transition",
    "request_id": "req_..."
  }
}
```

| 状況 | HTTP status |
|---|---:|
| invalid request framing/header | 400 |
| request body上限超過 | 413 |
| target unavailable/transition | 503 + `Retry-After` |
| upstream connect failure | 502 |
| upstream timeout | 504 |
| internal state failure | 500 |

Upstreamが返した4xx/5xxは原則そのまま返す。

## 11. Peer ingress

### 11.1 目的

Coordinatorだけが `10.99.0.1:18082` にpeer ingressをbindする。Worker public ingressはPaired Standalone/Distributed MXFP4時にここへ転送する。

Peer ingress handlerはmode-aware target resolverを再実行せず、coordinatorのlocal upstreamへだけ転送する。これによりproxy loopを構造上防ぐ。

### 11.2 認証

Worker proxyは次を付ける。

```text
X-DS4-Peer-Proxy-Token: <secret>
X-DS4-Proxy-Hop: 1
```

- Tokenは32 random bytes以上の生バイト列をmode `0600`のfileから読む。SSH秘密鍵やPEMではない。
- Coordinatorはconstant-timeで比較する。
- Hopが1以外、token不正、source IP不一致を403で拒否する。
- Peer ingressはtoken headerをlocal DS4へ転送しない。
- `Authorization` などoriginal client headerは保持する。
- Tokenをlog、metrics、error bodyへ出さない。
- Peer ingressはThunderbolt Bridge address以外へbindしない。

Request bodyをbufferせずstreamingするため、peer data planeはbody HMACを要求しない。DS4 native trafficも平文であることから、専用の信頼済みThunderbolt linkをsecurity boundaryとする。

### 11.3 Admission

Public ingressとpeer ingressはcoordinator local upstream用の同じadmission gateとin-flight counterを共有する。これによりlocal clientとworker clientの全DS4 requestをcoordinator processが一元的にdrainできる。

## 12. Admissionとdrain

### 12.1 Administrative state

```rust
enum AdmissionState {
    Serving,
    Draining,
    Blocked,
}
```

Available条件：

```text
admission_state == Serving
target upstream ready
```

### 12.2 Race-free gate

Permit取得と `Serving -> Draining` は同じ短時間のadmission mutexで直列化する。

Permit取得：

1. admission mutexをlock。
2. stateがServingか確認。
3. in-flight permitを取得。
4. mutexをunlock。
5. upstream network処理を開始。

Drain開始：

1. admission mutexをlock。
2. stateをDrainingへ変更。
3. mutexをunlock。
4. in-flight=0をawait。

Network await、response streaming、child exit待機中にadmission mutexを保持してはならない。

### 12.3 Local drain

DS4 child停止前に必ず次を行う。

```text
admission=Draining
  -> 新規requestは503
  -> in-flight=0待機
  -> admission=Blocked
  -> DS4へSIGTERM
  -> child exit待機
```

### 12.4 Cluster-wide drain

Paired StandaloneからDistributed MXFP4、またはDistributed MXFP4からPaired/Solo Standaloneへ切り替えるときはcoordinatorがdrain generationを開始する。

```text
coordinator:
  local public ingress + peer ingress admissionをDraining
  -> workerへBEGIN_DRAIN(generation)

worker:
  public ingress admissionをDraining
  -> worker側in-flight=0
  -> DRAINED(generation)を返す
  -> admission=Blocked

coordinator:
  coordinator側in-flight=0
  AND worker DRAINED受信
  -> owned DS4 child切替を開始
```

Worker requestはcoordinator peer ingressでもcountされるため、coordinator側in-flight=0がDS4に到達中の全requestの最終根拠となる。Worker ackは新しいrequestを生成しない保証として使う。

Drain timeoutを超えた場合、進行中requestを別modelで再実行しない。既定ではidentity確認済みowned childだけをSIGKILLできる。`allow_sigkill=false` ならmanual intervention。

## 13. Node roleとnetwork

### 13.1 IP over Thunderbolt readiness

起動時とnetwork change時に、System Configuration frameworkと `getifaddrs` からnetwork snapshotを作る。通常経路で `networksetup`、`scutil`、`ifconfig` のtext出力をparseしてはならない。

確認項目：

1. System Configuration preferences内に、BSD interface `bridge0` に対応するnetwork serviceが存在する。
2. `SCNetworkServiceGetEnabled` がtrueである。
3. IPv4 protocolが有効で、期待するaddress設定と矛盾しない。
4. Runtime stateに `bridge0` が存在し、UPである。
5. `bridge0` に期待するIPv4 address/prefixが付与されている。
6. Peer candidateへのrouteが `bridge0` scopedである。

Network serviceの表示名はuserが変更・localizeできるため、`Thunderbolt Bridge` という文字列だけで識別してはならない。実装がnetwork設定を作成、有効化、address変更してはならない。不備は診断情報として報告し、local standalone serviceを継続する。

```rust
enum ThunderboltIpState {
    ServiceMissing,
    ServiceDisabled,
    InterfaceUnavailable,
    AddressMissing,
    AddressConflict,
    ReadyNoPeer,
    PeerCandidateFound,
    AuthenticatedPeer,
}
```

`ReadyNoPeer` 以前の状態をpeer presentとして扱わない。`AuthenticatedPeer` だけがpairingを開始できる。

### 13.2 Role判定

`nix::ifaddrs::getifaddrs` で `bridge0` IPv4を取得する。

| address | role |
|---|---|
| `10.99.0.1` | coordinator |
| `10.99.0.2` | worker |
| その他/未設定/競合 | unknown |

外部command出力のparseを通常経路にしない。Role unknownではcluster control/peer ingress/distributedを開始しない。Local standaloneがreadyならpublic ingressはLocalStandaloneへ転送する。

### 13.3 Peer discovery

各nodeはcluster control listenerをBonjour service type `_ds4cluster._tcp`、domain `local.` としてadvertiseし、同じservice typeをbrowseする。既存HTTP listenerのportをそのまま登録し、interfaceを厳密に限定する必要があるため、AppleのDNS Service Discovery C APIを使用する。

- `if_nametoindex("bridge0")` でinterface indexを取得する。
- `DNSServiceRegister` でcontrol portをnetwork byte orderへ変換し、そのinterfaceだけに登録する。
- `DNSServiceBrowse`、`DNSServiceResolve`、`DNSServiceGetAddrInfo` の各resultで同じinterface indexを要求する。
- `DNSServiceRefSockFD` をTokio `AsyncFd` に統合し、readable時に `DNSServiceProcessResult` を呼ぶ。Blocking poll threadを増設しない。
- Registration/browse referenceはnetwork generationに所有させ、address変更時は古いreferenceをdeallocateして再作成する。

Bonjour TXT recordに含めてよいfield：

```text
protocol=1
node_id=<opaque stable id>
```

Role、mode、model、digest、token、secretをTXT recordから信用しない。自己advertisementを `node_id` で除外し、resolved addressが期待subnet上にあり、local addressではなく、routeが `bridge0` scopedであることを確認してpeer candidateとする。その後、既存HMAC control handshakeでnode descriptor、role、protocol versionを検証する。Bonjour発見だけではpeer presentにしない。

初期topologyではrole addressを `10.99.0.1` / `10.99.0.2` に固定する。Bonjourはpeerの出現を自動検出するが、DHCP addressからcoordinatorを自動選出する機能は含めない。将来DHCPを許可する場合は、stable node identity、deterministic election、address変更中のsplit-brain防止を別versionで仕様化する。

Bonjourが `NotPermitted`、`PolicyDenied`、daemon停止、registration failureで利用不能な場合は、設定済みcounterpart addressへのHMAC control接続をfallback discoveryとして使用できる。Packaged appとして配布する場合はlocal network usage descriptionとbrowseするBonjour service typeをmetadataへ宣言する。ICMP echoをdiscoveryまたはauthenticationの根拠にしない。

### 13.4 Listener

| Listener | Bind |
|---|---|
| public ingress | `127.0.0.1:18080` default |
| admin | `127.0.0.1:18081` default |
| peer ingress | coordinator `10.99.0.1:18082` only |
| cluster control | role address `:9920` |
| DS4 distributed | coordinator `10.99.0.1:9911` |
| DS4 HTTP | `127.0.0.1:8000` |

### 13.5 Cable/link event monitoring

System Configurationの `SCDynamicStoreSetNotificationKeys` を使い、少なくともinterface list、`bridge0` のLink/IPv4 state、対応network serviceのSetup stateを監視する。Callbackでは通知payloadだけを信用せず、500ms debounce後に第13.1節のnetwork snapshotを再取得してBonjour browseとcontrol handshakeを直ちに再評価する。通知の欠落に備え、30秒間隔の低頻度reconcileも行う。

Thunderbolt cable attach/detachは、Thunderbolt IP port、bridge member、Link、addressの複数eventとして遅延・重複・順不同で観測され得る。したがって単一eventを「peer接続済み」「peer切断済み」と解釈してはならない。

IOKitのpublish/terminate notificationを追加のwake-up hintまたはdiagnosticに使ってよい。ただし特定macOS versionのprivate driver class名やIORegistry propertyをcorrectness条件にしてはならない。物理的なThunderbolt deviceはdockやdisplayの場合もあるため、IOKit eventだけでpeer candidateを作らない。

状態遷移の根拠：

| Observation | Action |
|---|---|
| Link/address event | debounce後にsnapshot + browse再評価 |
| Bonjour candidate追加 | interface/route検証後HMAC handshake |
| HMAC handshake成功 + lease安定 | peer present、pairing候補 |
| Bonjour candidate消失のみ | lease expiryまでcurrent mode維持 |
| address消失、route消失、またはlease失効 | future admissionを閉じSolo Standaloneへ収束 |
| IOKit attach/detach hint | 即時rescanのみ。modeを直接変更しない |

通常LAN経由でpeer controlを確立してはならない。

## 14. Model profile

### 14.1 Profile

各nodeはstandalone profileを1つ選択する。実装は少なくとも次を表現可能でなければならない。

| standalone model variant | residency | DS4起動形態 |
|---|---|---|
| Q2 | `resident` または `ssd-streaming` | HTTP server |
| Q2-Q4（Q2を基礎に一部expert layerをQ4化） | `resident` または `ssd-streaming` | HTTP server |
| MXFP4 | `resident` または `ssd-streaming` | HTTP server |

`ssd-streaming` はDS4の `--ssd-streaming` を意味する。Model variantとresidencyを混同せず、たとえばMXFP4であることだけを理由にSSD streamingを暗黙有効化してはならない。逆にSSD streamingはQ2限定の機能として扱わない。

Standalone profileはnode固有でよく、coordinatorとworkerのmodel variant、residency、tuning値の一致をpairing条件にしない。Paired Standaloneではcoordinatorのprofileだけがrequestを処理し、peer喪失後のSolo Standaloneでは各nodeが自身のprofileを処理する。この差は `/cluster`、metrics、logで観測可能にする。

Distributed profileはstandalone profileとは独立した設定であり、初期実装では次を使用する。

| distributed profile | model | coordinator | worker |
|---|---|---|---|
| `distributed-mxfp4` | DeepSeek V4 Flash MXFP4 0731 | HTTP + distributed coordinator | distributed worker、HTTPなし |

### 14.2 Pathとstorage

- Modelはcanonical absolute pathで指定する。
- 書換可能なsymlinkをmodel pathに使わない。
- Distributedに用いる両nodeのMXFP4 content SHA-256を一致させる。
- 現行配布では約156GBのMXFP4 GGUFを両nodeへ配置する。
- 各distributed processは `--layers` で担当sliceだけをmapする。

### 14.3 Layer split

初期値：

```text
coordinator: 0:19
worker:      20:output
```

実測で変更可能だが、gap、overlap、layer 0欠落、output head欠落を拒否する。

### 14.4 SSD streaming

`residency = "ssd-streaming"` のstandalone profileでは、supervisorが `--ssd-streaming` を生成する。次のDS4 optionを型付き設定として公開し、`extra_args` からの重複指定を拒否する。

| 設定 | DS4 option | 規則 |
|---|---|---|
| `ssd_cache_experts` | `--ssd-streaming-cache-experts` | 正整数または `<number>GB`。省略可能 |
| `ssd_full_layers` | `--ssd-streaming-full-layers` | 0以上の整数。省略可能 |
| `ssd_preload_experts` | `--ssd-streaming-preload-experts` | 0以上の整数。省略可能。主に計測・warm-up用 |
| `ssd_cold` | `--ssd-streaming-cold` | default `false`。主にcold benchmark用 |

`residency = "resident"` では上記設定を指定してはならず、SSD streaming optionを生成しない。MXFP4 + SSD streamingを含む各組合せは設定として受理できることと、対象macOS/DS4 buildでproduction利用可能であることを分けて扱い、第32.5節のactual acceptanceをproduction gateとする。

### 14.5 DSpark

DSparkは現行DS4 baselineではresident Standaloneだけで使用する。`[ds4.dspark]`で`enabled = true`、canonicalかつ非symlinkのsupport GGUF、任意の`confidence`（0以上1以下）と`strict`を型付き指定する。Supervisorは`--mtp <support-model> --dspark`を一度だけ生成し、任意値から`--dspark-confidence`と`--dspark-strict`を生成する。

現DS4は`--ssd-streaming + --mtp`を拒否するため、DSpark有効時のStandaloneは`residency = "resident"`に限定する。DSpark optionはDistributed coordinator/worker argvへ追加しない。MXFP4 Distributedへの昇格中はDSpark非適用であり、DS4本体がdistributed supportを実装するまで有効化済みと表示しない。

Standalone childをspawnする前にsupport GGUFをstreaming SHA-256し、size、confidence、strictをStandalone manifestと照合する。不一致はfail closedとする。DS4が出力する`DSpark target-hidden capture enabled`をsanitized eventとして認識し、DSpark有効profileではHTTP readinessとこのeventの両方をstartup deadline内に要求する。Support pathとfull digestはadmin response/logへ出力しない。

### 14.6 KV cache

```text
Standalone:  ~/Library/Caches/ds4-kv/standalone/<profile-id>/
Distributed: ~/Library/Caches/ds4-kv/distributed/<deployment-id>/
```

Standaloneとdistributedでは異なる `--kv-disk-dir` を必須とする。同じMXFP4 GGUFを両方で使う場合も共有しない。基準DS4ではMXFP4も `quant_bits=2` と報告され得るため、quant bitsだけでcacheを区別しない。Mode変更後はclient transcriptから新deployment用KVを再構築し、別profileのsnapshotを直接loadしない。

## 15. Deployment manifest

### 15.1 Schema

```json
{
  "schema_version": 2,
  "profile": "distributed-mxfp4",
  "ds4_binary_sha256": "<node-local digest>",
  "compatible_ds4_binary_sha256": ["<coordinator digest>", "<worker digest>"],
  "ds4_source_commit": "<full Git object ID>",
  "model_sha256": "...",
  "model_size": 167503724544,
  "checkpoint": "flash-0731",
  "model_family": "deepseek-v4-flash",
  "quantization": "mxfp4-experts",
  "context_size": 262144,
  "coordinator_layers": "0:19",
  "worker_layers": "20:output",
  "ds4_wire_schema": "ds4d-v1-hello40",
  "argv_profile_sha256": "..."
}
```

Standalone manifestも同じdigest情報に加え、次を持つ。

```json
{
  "schema_version": 2,
  "profile": "standalone",
  "profile_id": "flash-0731-q2-q4-resident-dspark",
  "ds4_binary_sha256": "...",
  "model_sha256": "...",
  "checkpoint": "flash-0731",
  "model_variant": "q2-q4",
  "residency": "resident",
  "context_size": 262144,
  "argv_profile_sha256": "...",
  "dspark_enabled": true,
  "dspark_support_sha256": "...",
  "dspark_support_size": 5989114272,
  "dspark_confidence": 0.7,
  "dspark_strict": false
}
```

Standalone manifestはlocal childのidentity、設定drift、診断に使用する。DSpark有効時はsupport GGUFのdigest/sizeと挙動設定を必須とし、実file fingerprintおよびtyped configと一致しなければchildを起動しない。Peerへdescriptorとして通知してよいが、両nodeのstandalone profile不一致をpairing failureにしてはならない。

`compatible_ds4_binary_sha256` は実機acceptanceで相互運用を確認したbinary digestの、昇順・重複なしの集合である。各nodeの `ds4_binary_sha256` はこの集合に含まれなければならない。未知のrebuildや集合の片側だけの変更ではpromotionしない。

`deployment_id` はnode-localな `ds4_binary_sha256` を除くdistributed compatibility fieldを、key昇順、UTF-8、余分な空白なしのcanonical JSONにしてSHA-256した値とする。Local path、hostname、PID、log pathは含めない。同じ承認集合に属する異なるnative buildは同じdeployment IDを持つ。

### 15.2 Fingerprint

- DS4 binaryをproxy process起動時にSHA-256する。
- Modelは明示operator操作でstreaming SHA-256する。
- Device ID、inode、size、mtime、digest、計算日時をcacheする。
- 上記metadataが変わればstaleとし再fingerprintまでpromotionしない。
- 数百GBを読む処理をHTTP handler内で同期実行しない。
- Fingerprint jobは同一profileにつき1つ、job ID付き非同期処理とする。

### 15.3 Compatibility

Distributed profileは、承認済みbinary digest集合、full source commit、model digest/size、checkpoint、model family/quantization、context、layer split、wire schema、argv profileを比較する。各local binary digestが共通の承認集合に含まれ、これらのcompatibility fieldがすべて一致する場合だけMXFP4 promotionを許可する。deployment mismatch（control planeのHTTP 412）を検出した場合はpromotionを拒否し、両nodeをSolo Standaloneへ収束させる。自動pairingはoperator reconcileまたはruntime再起動まで再試行しない。

Binary digestはnode-local child identityと未知rebuildの検出に引き続き使用するが、cross-nodeでのbyte-for-byte一致は要求しない。`-mcpu=native`等による機種別binaryを承認集合へ追加するには、そのdigest pair、full source commit、wire schema、対象model/topologyでactual acceptanceを完了してcompatibility recordへ記録する。Source commitや自己申告のwire schemaだけで未知binaryを自動承認してはならない。

現行DS4 HELLOはprotocol version/source commit/build profileを通知しない。将来DS4がauthenticated handshakeでprotocol versionとcapabilityを通知できるまでは、承認済みbinary集合をoperator-controlled compatibility allowlistとして扱う。

## 16. Peer control protocol

### 16.1 Transport

HTTP/1.1 + JSON。各role address `:9920` にbindする。Body上限64KiB、connect timeout 1秒、request timeout 3秒。

### 16.2 HMAC authentication

必須header：

```text
X-DS4-Cluster-Node
X-DS4-Cluster-Timestamp
X-DS4-Cluster-Nonce
X-DS4-Cluster-Signature
```

署名対象：

```text
METHOD + "\n" +
PATH_AND_QUERY + "\n" +
TIMESTAMP_MILLISECONDS + "\n" +
NONCE + "\n" +
LOWERCASE_HEX_SHA256(BODY)
```

- Secret/token fileは32 random bytes以上の生バイト列、mode `0600`。標準のcanonical file名は
  拡張子なしの`cluster-control`、`peer-proxy`、`admin`とする。既存の`.key` fileはinstall時の
  移行対象として扱ってよい。
- Constant-timeで署名比較。
- Clock skew 30秒以内。
- Nonceを5分保持してreplay拒否。
- Source IPとnode IDを検証。
- Secret、署名、nonceを通常logへ出さない。

### 16.3 Endpoint

| Method | Path | 用途 |
|---|---|---|
| `GET` | `/v1/node` | node descriptor、mode、deployment、lease |
| `GET` | `/v1/metrics` | 認証済み peer から coordinator の Prometheus metrics を取得 |
| `POST` | `/v1/pair` | Paired Standalone形成 |
| `POST` | `/v1/prepare-worker` | MXFP4 worker準備 |
| `POST` | `/v1/begin-drain` | cluster-wide drain開始 |
| `POST` | `/v1/drained` | drain完了ack |
| `POST` | `/v1/cancel-generation` | 未完了transition中止 |
| `POST` | `/v1/worker-event` | worker child event push |
| `POST` | `/v1/distributed-ready` | complete route成立後にworker admissionを再開 |
| `POST` | `/v1/demote` | Paired/Solo Standaloneへの復帰要求 |

同一generationの再送はidempotent。古いgenerationは409、deployment不一致は412。`GET /v1/node` はDS4 inference health queryではなく、process membershipとcluster stateだけを返す。`GET /v1/metrics` は coordinator の role だけが応答し、認証済み worker からの read-only request に限定する。control plane の source IP、node ID、HMAC、nonce を検証し、route が bridge0 に scoped でない場合は拒否する。

### 16.4 Lease

初期leaseは15秒、renew intervalは5秒。Model load中もrenewする。Lease失効でfuture request admissionを閉じ、recovery stateへ移行する。

## 17. DS4 HELLO rendezvous

### 17.1 Sequence

Paired StandaloneでMXFP4 compatibilityが一致したら：

1. Coordinatorが `10.99.0.1:9911` でrendezvous listener開始。
2. Workerへprepare指示。
3. Worker proxyはcoordinator targetをdrainして一時Block。
4. Worker MXFP4 child起動。
5. 実DS4 childがHELLO送信。
6. RendezvousがHELLOを検証してsocket close。
7. Worker DS4は1秒間隔で再接続。
8. Coordinatorがcluster-wide drainを完了。
9. Coordinator standalone停止、MXFP4 coordinator起動。
10. Worker再接続、complete route形成。
11. 両proxy admission再開。

Cluster subsystemがHELLOを代理生成してはならない。

### 17.2 Wire schema

```rust
struct FrameHeader {
    magic: u32, // network byte order, 0x44533444 "DS4D"
    kind: u32,  // HELLO = 1
    bytes: u32,
}

struct HelloFixed {
    model_id: u32,
    quant_bits: u32,
    layer_start: u32,
    layer_end: u32,
    has_output: u32,
    has_hidden: u32,
    ctx_size: u32,
    n_layers: u32,
    listen_port: u32,
    model_name_len: u32,
}
```

Fixed payloadは40 bytes。`bytes == 40 + model_name_len`、`model_name_len <= 127`、read deadline 3秒を要求する。未知magic、type、size、trailing dataを拒否し、最初の1frameだけ読む。`quant_bits` をMXFP4 identityに使わない。

### 17.3 Acceptance condition

- StateがAwaitingWorkerHello。
- Source IPがworker address。
- Control lease/generationが有効。
- Distributed deployment ID一致。
- Expected layer、context、model family/nameと矛盾しない。
- Known wire fixtureとschema一致。

## 18. State transition

### 18.1 Boot to Solo Standalone

```text
Booting
  -> config/secret/manifest/state validation
  -> role detection
  -> orphan/owned child reconcile
  -> local standalone start
  -> HTTP readiness確認
  -> public target=LocalStandalone
  -> admission=Serving
  -> SoloStandaloneReady
```

### 18.2 Solo Standalone to Paired Standalone

Coordinator：

```text
SoloStandaloneReady
  -> authenticated peer stable
  -> coordinator standalone ready維持
  -> peer ingress Serving
  -> pair generation作成
```

Worker：

```text
SoloStandaloneReady
  -> coordinator peer ingress ready確認
  -> local admission drain
  -> local standalone stop
  -> target=Coordinator
  -> admission=Serving
  -> PairedStandaloneReady
```

Coordinator自身はtarget=LocalStandaloneのままPairedStandaloneReadyへ入る。

### 18.3 Paired Standalone to Distributed MXFP4

```text
PairedStandaloneReady
  -> deployment match
  -> rendezvous start / worker prepare
  -> actual HELLO received
  -> cluster-wide drain
  -> coordinator standalone stop
  -> MXFP4 coordinator start --debug
  -> worker registered
  -> complete route ready
  -> coordinator target=local MXFP4
  -> worker target=coordinator peer ingress
  -> both admission=Serving
  -> DistributedReady
```

HTTP listeningだけでDistributedReadyにしない。

### 18.4 Distributed to Paired Standalone

Peer controlは生きているがrouteが失われた場合：

```text
route loss grace経過
  -> cluster-wide drain
  -> MXFP4 children stop
  -> coordinator standalone start
  -> coordinator local + peer ingress ready
  -> worker target=Coordinator
  -> PairedStandaloneReady
```

### 18.5 Any paired mode to Solo Standalone

Peer lease/linkが失われた場合：

- Coordinatorは必要ならdistributedを停止してlocal standaloneを起動しSoloStandaloneReady。
- Workerはpublic admissionをBlockしlocal standaloneを起動後、targetをLocalStandaloneへ変更してSoloStandaloneReady。
- Peer通知不能でもlocal recoveryを妨げない。

Distributed deploymentの不一致を検出した場合も同じlocal recoveryを実行する。この場合はSoloStandaloneReadyへの遷移後も失敗理由を保持し、ユーザーへ構成不一致と自動pairing停止を通知する。原因を解消した後、operator reconcileまたはruntime再起動で自動pairingを再開する。

### 18.6 Failure/backoff

同一原因のpromotion失敗が3回連続したらauto promotionを止める。Standalone upstreamがreadyならproxyはServingを継続し、cluster stateだけManualInterventionRequiredとする。Operator reconcileまで再試行しない。

## 19. Timeout

| 項目 | 初期値 |
|---|---:|
| upstream connect | 5秒 |
| response headers | 60秒 |
| first body byte | 300秒 |
| stream idle | 300秒 |
| peer control connect | 1秒 |
| peer control request | 3秒 |
| control lease | 15秒 |
| peer stability | 5秒 |
| local/cluster drain | 180秒 |
| DS4 graceful stop | 180秒 |
| rendezvous HELLO | 900秒 |
| standalone startup | 900秒 |
| worker startup | 600秒 |
| coordinator startup | 600秒 |
| complete route | 180秒 |
| route loss grace | 15秒 |
| promotion backoff | 300秒 |

MXFP4 model mapは遅いためstartupより短いHELLO timeoutを設定しない。Timeoutは設定可能だが、0または無制限を拒否する。HTTP request全体timeoutは既定なしとする。

## 20. DS4 process supervision

### 20.1 Ownership

- DS4 childをTokio process APIで直接spawnしshellを介さない。
- Child用process groupを作る。
- PID、canonical executable、argv hash、profile、generation、spawn timestampを保存する。
- Process一覧のsubstring検索でidentityを決めない。
- PIDだけを信用しない。
- Unknown processへ無条件にsignalしない。起動時に `siderostat` / `ds4-server` の既存processを
  検出した場合は、macOSの右上通知（警告音付き）で簡潔に再起動を通知し、既定5秒後に
  起動時cleanupの対象にする。拒否は `startup_cleanup.auto_restart = false` または
  `--decline-startup-cleanup` で明示する。

### 20.2 Recovery

Proxy再起動時、macOS process APIでexecutable、argv、start timeを照合する。完全一致したowned childだけを
自動で再所有または停止対象にする。起動直前には同一ホストの `siderostat` / `ds4-server` を列挙し、
既存versionや外部launcher由来のprocessも候補にする。既定では簡潔な通知後5秒でcleanupを実行する。
`startup_cleanup.auto_restart = false` または `--decline-startup-cleanup` の場合は起動せず、
ManualInterventionRequired相当で停止する。通知を表示できない場合も既定の5秒動作は変えず、warnログを残す。
必要portを未知processが占有していても、cleanup対象としてidentityを再確認できない場合はsignalしない。

### 20.3 Signal

- 通常停止はSIGTERM。
- Drain完了後にsignalする。
- Stop timeout後のSIGKILLは既定で許可するが、identityを直前に再確認できたowned childだけを対象とする。`allow_sigkill=false` ならmanual intervention。
- 起動時cleanupは、5秒通知後の既定動作または明示的な拒否オプションとして扱う。
  SIGTERM/SIGKILLの各直前にPID、executable、argv hash、start timeを再確認し、通常のprocess groupではなく
  候補process自身へsignalする。
- Exit statusを必ず回収する。

### 20.4 Log event

Child stdout/stderrをnon-blockingで読み、profile、generation、PID、streamを付けて転送する。Distributed時は `--debug` 必須。

Recognized prefix：

```text
ds4-server: listening on http://...
ds4: distributed coordinator: registered worker ...
ds4: distributed coordinator: complete route ready: ...
ds4: distributed coordinator: removed worker ...
ds4: distributed coordinator: route incomplete; ...
ds4: DSpark target-hidden capture enabled: layers=...
```

専用parserで必要fieldを検証する。Unknown formatはlog転送だけ行いstateを進めない。

## 21. Readinessとhealth

### 21.1 DS4 readiness

| Profile | Ready condition |
|---|---|
| Standalone（Q2/Q2-Q4/MXFP4、各residency） | child生存、HTTP listening、`GET /v1/models`成功。DSpark有効時はactivation eventも必須 |
| MXFP4 coordinator | child生存、HTTP listening、worker registered、complete route |
| MXFP4 worker | child生存、実HELLO受信、lease有効 |

`GET /v1/models` はlocal managed DS4のnon-inference readinessだけに使う。Backend選択やload balancingには使わない。

### 21.2 Proxy readiness

`/readyz` はpublic ingressのcurrent targetがServing/readyなら200、それ以外は503。Coordinator peer ingressのready状態は `/cluster` に別fieldで返す。

### 21.3 Serving中failure

- Local upstream connect failureはcurrent requestへ502を返し、cluster recovery eventを発行する。
- Peer ingress connect failureはworker requestへ502を返し、peer lease/recoveryを再評価する。
- Automatic alternate target retryはしない。
- Client cancellationをDS4 failure countにしない。

## 22. Configuration

### 22.1 File resolution

TOML。探索順：

1. `--config PATH`
2. `SIDEROSTAT_CONFIG`
3. `./siderostat.toml`
4. platform default

Path先頭の `$VARIABLE`、`${VARIABLE}`、`~/` だけを展開する。Shell expansion、command substitutionは行わない。Unknown fieldを拒否する。

### 22.2 Complete example

```toml
schema_version = 2

[proxy]
public_listen = "127.0.0.1:18080"
admin_listen = "127.0.0.1:18081"
request_body_limit_bytes = 33554432
max_in_flight = 1

[proxy.timeouts]
connect = "5s"
response_headers = "60s"
first_body_byte = "300s"
stream_idle = "300s"

[cluster]
enabled = true
node_id = "macstudio-coordinator"
interface = "bridge0"
coordinator_address = "10.99.0.1"
worker_address = "10.99.0.2"
control_port = 9920
ds4_distributed_port = 9911
peer_ingress_port = 18082
state_path = "$HOME/Library/Application Support/siderostat/cluster-state.json"
manifest_cache_dir = "$HOME/Library/Application Support/siderostat/manifests"

[cluster.discovery]
mode = "bonjour-with-static-fallback"
bonjour_service_type = "_ds4cluster._tcp"
bonjour_domain = "local."
event_debounce = "500ms"
reconcile_interval = "30s"

[cluster.security]
control_secret_file = "$HOME/Library/Application Support/siderostat/secrets/cluster-control"
peer_proxy_token_file = "$HOME/Library/Application Support/siderostat/secrets/peer-proxy"
admin_token_file = "$HOME/Library/Application Support/siderostat/secrets/admin"
max_clock_skew = "30s"
nonce_ttl = "5m"

[cluster.policy]
auto_pair = true
auto_promote = true
auto_demote = true
required_peer_stability = "5s"
route_loss_grace = "15s"
promotion_backoff = "300s"
max_consecutive_promotion_failures = 3

[cluster.timeouts]
peer_connect = "1s"
peer_request = "3s"
control_lease = "15s"
drain = "180s"
stop = "180s"
rendezvous_hello = "900s"
worker_startup = "600s"
coordinator_startup = "600s"
complete_route = "180s"
standalone_startup = "900s"

[ds4]
binary = "$HOME/LLM/ds4/ds4-server"
working_directory = "$HOME/LLM/ds4"
http_host = "127.0.0.1"
http_port = 8000
allow_sigkill = true

[ds4.dspark]
enabled = true
support_model = "$HOME/LLM/ds4/gguf/DeepSeek-V4-Flash-DSpark-support-0731.gguf"
confidence = 0.7
strict = false

[ds4.standalone]
profile_id = "flash-0731-q2-q4-resident-dspark"
model = "$HOME/LLM/ds4/gguf/DeepSeek-V4-Flash-Layers37-42Q4KExperts-0731.gguf"
model_manifest = "$HOME/Library/Application Support/siderostat/manifests/standalone-flash-0731-q2-q4-resident-dspark.json"
checkpoint = "flash-0731"
model_variant = "q2-q4"
residency = "resident"
context_size = 262144
kv_disk_dir = "$HOME/Library/Caches/ds4-kv/standalone/flash-0731-q2-q4-resident-dspark"
kv_disk_space_mb = 262144
extra_args = []

[ds4.mxfp4]
model = "$HOME/LLM/ds4/gguf/DeepSeek-V4-Flash-MXFP4Experts-0731.gguf"
model_manifest = "$HOME/Library/Application Support/siderostat/manifests/mxfp4-0731.json"
checkpoint = "flash-0731"
context_size = 262144
coordinator_layers = "0:19"
worker_layers = "20:output"
kv_disk_dir = "$HOME/Library/Caches/ds4-kv/distributed/mxfp4-0731"
kv_disk_space_mb = 262144
extra_args = ["--debug"]

[logging]
format = "json"
level = "info"

[notifications]
enabled = true
sound = true

[startup_cleanup]
auto_restart = true
```

Worker nodeは `cluster.node_id` とnode固有pathだけを変更する。Roleはinterface addressから決定し、設定で直接指定しない。

### 22.3 Validation

- `schema_version == 2`。
- Public/admin/peer/control/distributed/DS4 portが衝突しない。
- `cluster.discovery.mode` が `bonjour`、`static`、`bonjour-with-static-fallback` のいずれかである。
- Bonjour service type/domainがDNS-SDのsyntaxと長さ制約を満たす。
- `event_debounce` と `reconcile_interval` が0/無制限でない。
- DS4 binaryがregular executable file。
- Model、DSpark support GGUF、manifestがregular file、canonical absolute path、書換可能symlinkでない。
- `model_variant` が `q2`、`q2-q4`、`mxfp4` のいずれかである。
- `residency` が `resident` または `ssd-streaming` である。
- `resident` ではSSD streaming設定が未指定またはdefault値、`ssd-streaming` ではsupervisorが `--ssd-streaming` を一度だけ生成する。
- DSpark有効時はsupport modelが必須、confidenceは0以上1以下、Standaloneは`resident`である。
- Standalone/distributedのKV directoryが異なる。同じmodel pathであっても共有しない。
- Layer splitにgap/overlapがなく、workerがoutputを所有する。
- Secret/token fileが各32 bytes以上、mode `0600`、相互に異なる。
- Timeoutが0/無制限でない。
- `extra_args` が生成引数を上書きしない。

`extra_args` で禁止：

```text
-m / --model
--role
--layers
--coordinator
--listen
--host
--port
--ctx
--kv-disk-dir
--kv-disk-space-mb
--ssd-streaming
--ssd-streaming-cache-experts
--ssd-streaming-full-layers
--ssd-streaming-preload-experts
--ssd-streaming-cold
--mtp
--dspark
--dspark-confidence
--dspark-strict
```

### 22.4 Legacy configuration

Version 1の `backends`、`routing`、`affinity`、`heartbeat`、`active_probe`、`cooldown`、SQLite pathはtarget仕様で廃止する。

- Unknown/legacy fieldを黙って無視しない。
- Actionableなmigration errorを返す。
- 旧affinity databaseを自動削除しない。
- Config migration commandを提供してよいが、曖昧な複数backend設定はoperator選択なしに変換しない。
- Public/admin listenとtimeoutは可能な限り機械変換できる。

## 23. Admin APIとCLI

### 23.1 Admin API

Loopback `127.0.0.1:18081` default。

| Method | Path | 内容 |
|---|---|---|
| `GET` | `/healthz` | process liveness |
| `GET` | `/readyz` | current target readiness |
| `GET` | `/cluster` | role、mode、state、generation、target、lease、child、Thunderbolt IP/discovery状態、active standalone profile ID/model variant/residency |
| `GET` | `/metrics` | Prometheus text |
| `GET` | `/metrics/coordinator` | worker が control plane 経由で取得した coordinator の Prometheus text |
| `POST` | `/cluster/reconcile` | observed stateをdesired stateへ収束 |
| `POST` | `/cluster/pair` | Paired Standaloneを要求 |
| `POST` | `/cluster/promote` | MXFP4 promotion要求 |
| `POST` | `/cluster/demote` | standalone demotion要求 |
| `POST` | `/cluster/restart` | current profile child再起動 |
| `POST` | `/cluster/fingerprint` | async model fingerprint job |

GETはsecretを返さない。Mutationはloopbackでもadmin token必須。Fingerprintは202 + job IDを返し、同一profileの同時jobを拒否する。

### 23.2 CLI

既存起動形式との互換性：

```text
siderostat --config PATH
siderostat serve --config PATH
```

Subcommandなしは `serve` と同じ。

```text
siderostat cluster status [--json]
siderostat cluster doctor [--json]
siderostat cluster reconcile
siderostat cluster pair
siderostat cluster promote
siderostat cluster demote [--reason TEXT]
siderostat cluster restart
siderostat cluster fingerprint --profile standalone|distributed
```

CLIはrunning processのadmin API clientであり、別supervisorを起動しない。Status/doctorはread-only。Restartはmodeを変えず、promoteはHELLO/compatibility条件を迂回しない。

## 24. Persistent state

Affinity/backend selection stateは保存しない。Cluster lifecycleだけをJSONへ保存する。

```json
{
  "schema_version": 1,
  "generation": 43,
  "desired_mode": "distributed-mxfp4",
  "last_stable_mode": "paired-standalone",
  "cluster_state": "distributed-ready",
  "proxy_target": "coordinator",
  "active_profile": "distributed-mxfp4-coordinator",
  "child": {
    "pid": 12345,
    "executable": "/absolute/path/ds4-server",
    "argv_sha256": "...",
    "spawned_at": "..."
  },
  "last_failure": null
}
```

- Temp write、file sync、atomic rename。
- Secret/tokenを保存しない。
- Generationをwall clockだけで生成しない。
- Single-instance file lockを取得。
- Corrupt stateを保全し、安全確認可能ならSolo Standalone、不能ならManualInterventionRequired。
- 旧SQLite affinity databaseを読まない、変更しない、削除しない。

## 25. Logging

### 25.1 Request log

```text
timestamp
level
request_id
ingress=public|peer
method
path_template
proxy_target=local-standalone|coordinator|unavailable
cluster_mode
cluster_state
generation
in_flight_before
upstream_connect_ms
response_header_ms
first_body_byte_ms
total_ms
status
bytes_in
bytes_out
error_kind
```

### 25.2 Cluster event

```text
proxy_started
role_changed
thunderbolt_ip_state_changed
network_change_observed
peer_candidate_discovered
peer_candidate_rejected
solo_standalone_ready
peer_discovered
pairing_started
paired_standalone_ready
deployment_mismatch
promotion_started
worker_prepare_accepted
ds4_hello_received
cluster_drain_started
cluster_drain_completed
distributed_route_ready
route_lost
demotion_started
fallback_ready
child_exited
manual_intervention_required
```

### 25.3 Redaction

次をlogしない。

- Authorization/API key/Cookie
- request/response body
- prompt、session/conversation ID
- peer proxy token
- HMAC secret/signature
- 完全model digest/deployment ID

Digestは先頭12 hexだけをtagとして使用可能。

## 26. Metrics

```text
ds4_proxy_requests_total{ingress,target,status_class}
ds4_proxy_request_duration_seconds{target}
ds4_proxy_time_to_first_byte_seconds{target}
ds4_proxy_in_flight{ingress,target}
ds4_proxy_target_ready{target}
ds4_proxy_upstream_failures_total{target,reason}
ds4_proxy_cluster_state{node_id,state}
ds4_proxy_cluster_mode{node_id,mode}
ds4_proxy_cluster_generation{node_id}
ds4_proxy_cluster_peer_lease_seconds{node_id}
ds4_proxy_thunderbolt_ip_state{node_id,state}
ds4_proxy_peer_discovery_results{node_id,interface}
ds4_proxy_peer_discovery_events_total{source,result}
ds4_proxy_cluster_transitions_total{from,to,result,reason}
ds4_proxy_cluster_transition_duration_seconds{transition}
ds4_proxy_cluster_child_restarts_total{profile,reason}
ds4_proxy_standalone_profile_info{node_id,model_variant,residency}
ds4_proxy_cluster_hello_total{result,reason}
ds4_proxy_cluster_deployment_mismatch_total{field}
ds4_proxy_ds4_prefill_active
ds4_proxy_ds4_prefill_current
ds4_proxy_ds4_prefill_total
ds4_proxy_ds4_prefill_percent
ds4_proxy_ds4_prefill_cached
ds4_proxy_ds4_prefill_chunk_tps
ds4_proxy_ds4_prefill_avg_tps
ds4_proxy_ds4_prefill_elapsed_seconds
ds4_proxy_ds4_kv_cache_hits_total
ds4_proxy_ds4_kv_cache_hit_tokens
ds4_proxy_ds4_kv_cache_load_ms
ds4_proxy_ds4_generation_active
ds4_proxy_ds4_generation_completion
ds4_proxy_ds4_generation_chunk_tps
ds4_proxy_ds4_generation_avg_tps
ds4_proxy_ds4_generation_elapsed_seconds
```

`model_variant` と `residency` は設定で許可した有限enumだけをlabel値にする。Profile ID、session、request ID、PID、generation、full digestをlabelにしない。

## 27. Security

- Public/adminはloopback default。
- Peer ingress/control/DS4 distributedはThunderbolt Bridgeだけにbind。
- Peer ingressはsource IP、token、hopを検証。
- Control planeはHMAC、timestamp、nonce、source IPを検証。
- Control secretとpeer tokenはそれぞれ両nodeで同じ値を使う。Control、peer、adminの用途間で値またはfileを流用しない。
- Secret/token fileは32 bytes以上、mode `0600`。
- DS4 HTTPはloopbackだけ。
- Shellを介してchildをspawnしない。
- Config argvを1要素ずつ `Command::arg` へ渡す。
- Unknown processを無条件ではkillしない。起動時は簡潔な通知と5秒の既定猶予を置き、拒否指定がない場合だけ
  signal直前のidentity再確認後にcleanupする。拒否は `startup_cleanup.auto_restart = false` または
  `--decline-startup-cleanup` で指定する。
- Model fingerprint時にregular file/canonical pathを確認。
- Admin mutationはtoken必須。
- Prompt/body/tokenをlogしない。

DS4 native distributed trafficとpeer proxy bodyは暗号化されない。専用の物理Thunderbolt linkを信頼境界とする。

## 28. Concurrency

- Public/peer ingressのcoordinator local targetは同一admission gate/counterを共有。
- Network await中にadmission/cluster global lockを保持しない。
- Control handlerはbounded channelでstate machineへevent送信し、handler内でchildを操作しない。
- State transitionはsingle writer taskが直列処理。
- Log readerはchildをbackpressureさせないbounded/overflow policyを持つ。
- State file writeとrequest path lockを分離。
- Drain generationとcluster generationを照合し、old ackを無視。
- Cancellation storm後にin-flightが0へ戻ることを保証。

## 29. Dependencies

### 29.1 Retain

- `tokio`：runtime、TCP、process、signal、timer、channel。
- `axum`：public/peer/admin/control HTTP server。
- `hyper` / `hyper-util`：streaming forwarding。
- `tower` / `tower-http`：HTTP middleware。
- `reqwest` またはHyper client：upstream/control client。
- `bytes` / `futures`：streaming body。
- `clap`：serve/cluster CLI。
- `serde` / `serde_json` / `toml`：config/protocol/state。
- `tracing` / `tracing-subscriber`：structured logging。
- `anyhow` / `thiserror`：error。
- `hmac` / `sha2`：control auth、digest。
- `uuid`：request ID/nonce。

### 29.2 Add

- `nix` with `fs`, `net`, `process`, `signal`：interface、process group、signal、file lock。
- `libc`：safe wrapperで不足するmacOS process identity APIだけ。
- System Configuration bindings：service enabled/configuration snapshotとDynamic Store notification。`system-configuration-sys` または `objc2-system-configuration` のどちらか一方をPhase 0で評価し、必要API coverage、memory ownership、Dispatch integrationが明確な方を採用する。
- DNS Service Discovery C API bindings：`DNSServiceRegister/Browse/Resolve/GetAddrInfo` とTokio `AsyncFd` integration。必要symbolだけの小さなproject-owned FFI moduleを優先し、第三者crateを採用する場合はsource audit、macOS 26 CI、callback lifetime/cancellation安全性をPhase 0 gateとする。

同じApple frameworkに対する複数binding stackを併用しない。Shell command pollingをfallback implementationにしない。

### 29.3 Remove after migration

- `rusqlite`：affinity persistence廃止。
- `unicode-normalization`：affinity key normalization廃止。
- `url`：直接利用箇所がなくなった場合だけ削除。

Dependency major updateをmode-aware migrationと同じchangeに含めない。既存versionとCargo.lockを基準にし、必要なcrateだけ追加/削除する。

## 30. Source layout

```text
src/
  main.rs
  app.rs
  config.rs
  error.rs
  proxy.rs
  metrics.rs
  target.rs
  admission.rs
  cluster/
    mod.rs
    config.rs
    role.rs
    network_snapshot.rs
    network_events.rs
    discovery.rs
    bonjour.rs
    manifest.rs
    auth.rs
    control.rs
    state.rs
    state_store.rs
    coordinator.rs
    worker.rs
    process.rs
    ds4_command.rs
    ds4_log.rs
    ds4_hello.rs
```

削除候補：

```text
affinity.rs
routing.rs（target.rsへ置換）
heartbeat.rs（local readinessをclusterへ移動）
persistence.rs（cluster state_storeへ置換）
```

一度に削除せず、target resolverと新configがtestで置換された後にdead code/dependencyを除去する。

## 31. Failure policy

| Failure | Behavior |
|---|---|
| peer不在 | Solo Standalone |
| Thunderbolt Bridge service missing/disabled | 設定を変更せずSolo Standalone、doctorで修復案を表示 |
| `bridge0` address missing/conflict | Role unknown、cluster listener停止、local standalone維持 |
| Bonjour unavailable | static fallbackが有効ならHMAC接続を試行、無効ならSolo Standalone |
| Unauthenticated Bonjour result | candidate破棄、current mode維持 |
| peer control HMAC不正 | request拒否、Solo/Paired current mode維持 |
| peer proxy token不正 | peer ingress 403 |
| deployment mismatch | Solo Standaloneへ収束、MXFP4 promotion禁止、自動pairing停止 |
| manifest stale | Paired Standalone、再fingerprint要求 |
| HELLO timeout | MXFP4 worker停止、Paired Standalone、backoff |
| unknown HELLO/log schema | promotion拒否、Paired Standalone |
| coordinator startup timeout | MXFP4停止、Paired Standalone |
| route incomplete/lost | grace後Paired Standalone |
| peer link/lease loss | 両node Solo Standaloneへ収束 |
| child identity不明 | signal禁止、ManualInterventionRequired |
| local standalone start失敗 | target Unavailable、ready=false |
| drain timeout | policyによりmanualまたはverified child kill |
| state corrupt | 保全、安全ならSolo Standalone、不能ならmanual |

## 32. Test strategy

### 32.1 Unit

- Config v2 parse/unknown/legacy field rejection。
- Standalone `model_variant` × `residency` matrixのparse/argv生成。
- `resident` とSSD streaming設定の矛盾、およびSSD option重複の拒否。
- Path expansionとsecret permission。
- Role/address判定。
- Thunderbolt IP readiness state全組合せ。
- System Configuration eventのdebounce/coalescingとperiodic reconcile。
- Bonjour self-filter、wrong interface/subnet/route、duplicate result拒否。
- Mode-to-target table全組合せ。
- Admission/drain race。
- In-flight RAII/cancellation。
- Peer ingress token/hop/source validation。
- HMAC、clock skew、nonce replay。
- Layer gap/overlap。
- Manifest canonicalization/stale cache。
- HELLO endian/length/magic/type/truncation。
- DS4 log parser。
- Generation/idempotency/old ack拒否。
- Process identity mismatch signal拒否。
- Atomic state recovery。
- Error mapping/header redaction。

### 32.2 Property/fuzz

- Arbitrary bytesでHELLO parserがpanicしない。
- Arbitrary HTTP headerでhop-by-hop除去がpanicしない。
- Manifest key orderでdeployment IDが変わらない。
- HMAC field境界変更でverify失敗。
- Arbitrary state event sequenceが複数targetを同時選択しない。

### 32.3 Integration with fake DS4

1. 各node Solo Standalone/Public target local。
2. Peer認証後、workerをdrainしてPaired Standalone。
3. Worker public requestがcoordinator peer ingress経由でlocal standaloneへ届く。
4. Peer ingressがinvalid token/hop/sourceを拒否。
5. deployment mismatchでは構成不一致を通知し、両nodeがSolo Standaloneへ収束してstandalone serviceを継続する。
6. Compatible deploymentで実HELLO受信。
7. Cluster-wide drain中、新規requestは503、既存streamは完走。
8. Coordinator側in-flight=0までDS4を停止しない。
9. Complete route後、両node targetがcoordinator。
10. Worker exit/route lossでPaired Standalone。
11. Peer lossで両node Solo Standalone。
12. Connect failureをalternate modelへretryしない。
13. Proxy process listenerが全transitionで維持される。
14. Crash/restartでowned child reconcile。
15. Unknown PIDへsignalなし。
16. 旧generation message/ack無視。
17. standalone/distributed KV directory混在なし。
18. Link/address eventからdebounce後にBonjour rescan。
19. Bonjour発見だけではpairingせず、HMAC成功後だけpeer present。
20. Bonjour result消失だけでは即時demoteせず、lease失効後にdemote。
21. Cable detach相当のaddress/route喪失でSolo Standaloneへ収束。
22. Bonjour failure時にstatic fallback、復旧時に重複pairingなし。

### 32.4 Streaming

- 100ms間隔SSE chunkを同順序/同程度間隔で転送。
- Worker -> peer ingress -> DS4の2 hopでもfull bufferingなし。
- Client cancelでworker/coordinator両proxyのpermit解放。
- Mid-stream failureでretryなし。
- Memory usageがresponse sizeに比例しない。

### 32.5 macOS actual acceptance

- M4 Max 128GB coordinator、M5 Max 128GB worker、Thunderbolt 5直結。
- 同一full DS4 source commit/wire schemaと、相互運用を実測して承認したnode別binary digest集合。MXFP4 0731 digestは同一。
- 利用対象のresident standaloneでrequest成功。現行targetはQ2-Q4 residentとし、Q2は対応するfull standalone modelが配置された場合に追加確認する。
- Q2-Q4 SSD streaming standaloneでrequest成功。
- MXFP4 SSD streaming standaloneでrequest成功。対象DS4 build/Metal backendで未確認の場合は、そのprofileだけをproduction enable不可とする。
- 両nodeが異なるstandalone profileでもpairingでき、worker requestがcoordinatorのprofileで処理される。
- Pairing後worker requestがcoordinator standaloneで成功。
- 実HELLO/complete route確認。
- MXFP4 short prompt/8K以上prefill成功。
- Memory pressure/startup time許容。
- Cable disconnectで両node local standalone復帰。
- Reconnect/backoff後Paired Standalone、次いでMXFP4再昇格。
- Proxy起動中に2回連続で実cable着脱し、eventごとにpolling待ちなしでrescanが開始され、orphan transitionがない。反復耐久性はfake route detach/attach 10回とpromotion/demotion 10回の自動testで補完する。
- Thunderbolt dockだけの着脱ではpeer presentにならない。
- Bonjour advertisementを通常LANでも観測できる環境で、`bridge0` 以外のresultを拒否する。
- Login起動、proxy restart、child restart成功。
- 10回promotion/demotionでorphan/port残留/PID誤killなし。
- Peer ingress追加hopを含むproxy overhead p50 5ms未満を目標。

設定parserが組合せを受理することは、そのmodel/backend/residencyの動作保証を意味しない。特にMXFP4 SSD streaming on MetalとMXFP4 distributedはactual acceptanceをproduction gateとする。

## 33. Acceptance criteria

### 33.1 Proxy

- [ ] Public ingressが全modeで同じaddress/portを維持。
- [ ] Unknown OpenAI-compatible pathを透過中継。
- [ ] Request/responseをfull bufferしない。
- [ ] SSE order/timingを保持。
- [ ] Modeだけでtargetが一意。
- [ ] Load balancing/affinity/alternate retry codeがrequest pathにない。
- [ ] Unavailable/transitionは503 + Retry-After。

### 33.2 Pairing

- [ ] Peer不在はlocal standalone。
- [ ] IP over Thunderboltのservice enabled、interface、address、routeをAPIで確認。
- [ ] `bridge0` のchange eventでdebounced peer discoveryを即時再評価。
- [ ] Bonjour advertisement/browseが `bridge0` に限定される。
- [ ] Bonjour resultだけではpairingせずHMAC/leaseを必須とする。
- [ ] IOKit eventまたはICMPだけでmodeを変更しない。
- [ ] Authenticated peer存在時はworker targetがcoordinator。
- [ ] Standalone profileがnode間で異なってもpairingできる。
- [ ] Deployment mismatchでもPaired Standaloneが利用可能。
- [ ] Peer ingressはauth/hop/sourceを検証。
- [ ] Proxy loopが構造上不可能。

### 33.3 Distributed

- [ ] Binary/model/checkpoint/argv一致を検証。
- [ ] 実HELLOなしでpromotionしない。
- [ ] Complete route前にadmission再開しない。
- [ ] Cluster-wide drain後だけDS4を停止。
- [ ] standalone/distributed KV directory分離。
- [ ] Route lossでPaired Standaloneへ復帰。
- [ ] Peer lossでSolo Standaloneへ復帰。

### 33.4 Safety/operations

- [ ] Unknown processは無承認でsignalせず、起動時の候補提示・operator承認・signal直前のidentity再確認を行う。
- [ ] Secret/body/session IDをlogしない。
- [ ] Admin mutation token必須。
- [ ] State atomic recovery。
- [ ] Metrics/logでtransition原因を確認可能。
- [ ] `cargo fmt --check` 成功。
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` 成功。
- [ ] `cargo test --all-targets` 成功。
- [ ] macOS actual acceptance成功。

### 33.5 Standalone profile

- [ ] Q2、Q2-Q4、MXFP4を設定可能。
- [ ] `resident` と `ssd-streaming` をmodel variantから独立して設定可能。
- [ ] SSD streamingの型付き設定から正しいDS4 argvを一度だけ生成。
- [ ] Standalone/distributedが同じGGUFでもKV namespaceを共有しない。
- [ ] Active profile ID、model variant、residencyをadmin API、metrics、logで確認可能。

## 34. Implementation plan

v0.1.0の実装順序、migration、verification、rollback記録は [`implementation-plan-v0.1.0.md`](archive/implementation-plan-v0.1.0.md) にarchiveした。本書はtarget behaviorとacceptance criteriaを定義し、作業進捗やcommit分割を管理しない。

## 35. macOS service

1つのuser service jobだけがproxy processを管理する。

```text
macOS user service manager
  -> siderostat serve
       -> owned ds4-server child
```

- `RunAtLoad=true`。
- `KeepAlive=true`。
- Absolute ProgramArguments。
- Finite restart throttle。
- Secret/tokenをjob definitionへ直接書かない。
- DS4 childを別jobへ登録しない。
- 同じport/processを複数jobから管理しない。

## 36. Upstream compatibility tracking

DS4 update時に確認する。

| Target | Impact |
|---|---|
| `ds4_distributed.c/.h` | HELLO/wire/reconnect/route event |
| model ID/name/quant/layer APIs | compatibility/split |
| server signal handling | drain/stop |
| server/distributed log | readiness parser |
| GGUF/checkpoint | manifest/KV namespace |
| CLI option | generated argv |
| distributed QA | MXFP4 gate |

Repository内にverified DS4 commit、binary digest、wire/log fixture digest、recognized event、tested model/topology、dateを記録する。Unknown changeではpromotionをfail closedにする。

## 37. Initial limitations

- standalone/distributed切替中は503の短いwindowがある。
- Workerからcoordinatorへのrequestはproxyを2 hop通る。
- Peer data/DS4 native trafficは暗号化されない。
- 2 node固定。
- Peer discoveryは自動だがrole addressは `10.99.0.1` / `10.99.0.2` 固定で、DHCP based electionは行わない。
- standalone/distributed間でlive KVを引き継がない。
- DS4 log textへの依存がある。
- MXFP4 SSD streaming on MetalとMXFP4 distributedのproduction可否はactual acceptance結果で決める。

## 38. Definition of Done

- 第33章の全acceptance criteriaを満たす。
- Legacy load balancer/affinity persistenceがrequest pathから除去される。
- Config v2 complete exampleが実parserで成功する。
- Fake DS4 integrationが全transitionを通過する。
- Unknown HELLO/PIDをfail closedにする。
- standalone/distributed cacheを分離する。
- Peer/route喪失からstandaloneへ自動復帰する。
- Q2、Q2-Q4、MXFP4のstandalone profileとSSD streaming argv生成をtestする。
- System Configuration eventとBonjour discoveryでcable再接続後に自動pairingへ収束する。
- Bonjour/IOKit/ICMPだけをtrustせず、`bridge0` route、HMAC、leaseを必須とする。
- Public proxy listenerがtransition中も維持される。
- macOS actual acceptanceが成功する。
- Operator rollback手順とupstream compatibility記録が存在する。

## 39. References

- [DS4 repository](https://github.com/antirez/ds4)
- [DS4 distributed inference](https://github.com/antirez/ds4#distributed-inference-with-pipeline-parallelism)
- [Apple: ThunderboltでIPを使ってMacコンピュータを接続する](https://support.apple.com/ja-jp/guide/mac-help/mchld53dd2f5/mac)
- [Apple Developer: System Configuration](https://developer.apple.com/documentation/systemconfiguration)
- [Apple Developer: SCDynamicStore](https://developer.apple.com/documentation/systemconfiguration/scdynamicstore-gb2)
- [Apple Developer: DNS Service Discovery](https://developer.apple.com/documentation/dnssd)
- [Apple Developer: DNS Service Discovery C API](https://developer.apple.com/documentation/dnssd/dns-service-discovery-c)
- [Apple Developer: IOServiceAddMatchingNotification](https://developer.apple.com/documentation/iokit/1514362-ioserviceaddmatchingnotification)
