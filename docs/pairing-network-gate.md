# Pairing network gate (N-01)

本設計は `docs/reconnect-improvement-implementation-plan.md` の **Phase N (P2 route /
discovery pairing gate)** の正本である。対象は N-01（pairing gate の network evidence
contract の固定）で、現行 `production` handler が `route_scoped=true` を固定値で渡している
問題を明記し、N-02 で `NetworkSnapshot` / `DiscoveryTracker` の実測 evidence へ置換する
契約を定義する。詳細な観測は計画書 R0-04/05/06 Evidence と B-01〜B-04 Evidence を参照。

## 1. 現行の不整合

現行の production control handler は `route_scoped` を実測せず、**固定値 `true`** を
control 層へ渡している。

`src/cluster/production/effects.rs` の `ProductionClusterRuntime::handle()` が
`RoleControl::Coordinator` / `RoleControl::Worker` に対して、次の 4 箇所で `true` を
ハードコードしている（60, 66, 79, 93 行目付近）:

- `/v1/node` 応答: `.node_descriptor(&authenticated, true, now)`
- 各 control message: `.handle(endpoint, message, &authenticated, true, now)`

一方、`src/cluster/control.rs` の `PeerLease` は `route_scoped` を状態として持ち、
`peer_present()` は `route_scoped && stable && !expired && descriptor.is_some()` で判定する。
`establish()` / `renew()` は `route_scoped=false` なら `ControlError::RouteNotScoped` を
返す。

つまり **production では route scoping が常に真と仮定されており、`bridge0` scoped route
の実測が control lease / pairing 判定へ一切効いていない**。spec 第9.3節・第13.1節が要求する
「peer candidate への route が `bridge0` scoped であること」の検証が production 経路では
欠落している。N-01 はこの契約を固定し、N-02 で実測値へ置換する。

## 2. 正本と参照

| 項目 | 正本 |
|---|---|
| peer presence の意味 | `docs/spec.md` §9.3 |
| IP over Thunderbolt readiness | `docs/spec.md` §13.1 |
| Peer discovery | `docs/spec.md` §13.3 |
| Cable/link event monitoring | `docs/spec.md` §13.5 |
| Control lease | `docs/spec.md` §16.4 |
| 既存 network observation 実装 | `src/cluster/network_snapshot.rs` |
| 既存 discovery 実装 | `src/cluster/discovery.rs`、`src/cluster/bonjour.rs` |
| 既存 event monitor 実装 | `src/cluster/network_events.rs` |
| control lease / peer_present | `src/cluster/control.rs` |
| production handler（固定 `true`） | `src/cluster/production/effects.rs` |
| harness 設計判断の根拠 | 計画書 R0-04/05/06 Evidence |

## 3. peer present の必須条件

spec 第9.3節・第13.1節・第13.3節に基づき、**peer present**（pairing を開始できる状態）は
次の 6 条件を**全て**満たす。いずれか一つでも欠ける場合は peer present にしない。

| # | 条件 | 検証源 | 現行の担保状況 |
|---|---|---|---|
| 1 | `bridge0` に期待する local address（`10.99.0.1`/`10.99.0.2`）と prefix がある | `NetworkSnapshot::from_observation`（`assess_role`）、`RoleAssessment::Known` | 実測あり |
| 2 | peer の remote IP が期待 peer address と同一 subnet 上にある | `DiscoveryTracker::accept_bonjour`（`WrongSubnet`/`UnexpectedAddress`）、`assess_role` の prefix 照合 | 実測あり |
| 3 | peer candidate への route が `bridge0` scoped である | `NetworkSnapshot` の `PeerObservation.route_scoped_to_interface`、`DiscoveryTracker` の `RouteNotScoped` | **production では固定 `true`（未実測）** |
| 4 | HMAC 認証済み node descriptor を受信できる | `ProductionClusterRuntime::authenticate` → `SignedControlHeaders` 検証 | 実測あり |
| 5 | control lease が有効（未失効） | `PeerLease::expired()`（lease 15s / renew 5s） | 実測あり |
| 6 | `required_peer_stability`（5s）の間、上記が継続 | `PeerLease::peer_present()` の `stable`、`first_authenticated_at_millis` | 実測あり |

**発見条件（discovery candidate）の必須条件**は `DiscoveryTracker::accept_bonjour` の検査
順序に固定する。次の全てを満たした場合のみ candidate として保持する。

| 検査 | 失敗時 |
|---|---|
| `generation` 一致 | `CandidateError::OldGeneration` |
| `node_id` / address が self でない | `CandidateError::SelfResult` |
| `interface_index` 一致 | `CandidateError::WrongInterface` |
| `protocol_version` 一致（=1） | `CandidateError::WrongProtocol` |
| `port != 0` | `CandidateError::InvalidPort` |
| 同一 subnet | `CandidateError::WrongSubnet` |
| address が期待 peer address | `CandidateError::UnexpectedAddress` |
| `route_scoped_to_interface` | `CandidateError::RouteNotScoped` |

static fallback は `BonjourFailure::allows_static_fallback()`（`NotPermitted` /
`PolicyDenied` / `DaemonUnavailable` / `RegistrationFailed`）のときだけ許可し、
`port != 0` かつ route scoped でなければ candidate にしない（`StaticFallbackNotAllowed` /
`RouteNotScoped`）。

## 4. snapshot / candidate の generation / epoch と古い観測の拒否

**設計判断**: 観測（observation）と candidate には、cluster/session とは独立した
**観測 epoch**（単調増加の rescans ごとに +1 する counter）を付与し、pairing 判定に使う
観測が要求時点より古くないことを要求する。

- 既存の candidate は `DiscoveryInput.generation` / `ResolvedBonjourService.generation` /
  `NetworkEvent.generation` / `BonjourRegistration.generation` / `BonjourLifecycle.generation`
  で **generation** を保持しており、古い generation の観測は `OldGeneration` で拒否される
  （`DiscoveryTracker::accept_bonjour`、`network_events.rs` の generation フィルタ、
  `BonjourLifecycle::accepts`）。
- `NetworkObservation` / `NetworkSnapshot` は現行 **epoch を持たない**。N-01 の契約として、
  N-02 で `NetworkSnapshot` に観測 epoch を追加し、snapshot を pairing 判定へ渡す際は
  request 時点の epoch と一致（またはそれ以降に完了した rescan 由来）であることを検証する。
- 古い epoch の snapshot / candidate は pairing 判定へ使わない。古い観測で新しい
  lease / state を上書きしない。

**失効条件**: epoch は rescan ごとに進む。ある epoch の snapshot は、次の rescan が完了
した時点で stale になる。stale snapshot による判定は受理しない。

## 5. Bonjour 単独では peer present にしない

Bonjour（または static fallback）で得た candidate は **peer candidate** に過ぎず、
それだけでは peer present にしない。

- `NetworkSnapshot::from_observation` は `candidate_valid &&
  observation.peer.authenticated` のときだけ `ThunderboltIpState::AuthenticatedPeer` とし、
  `peer_present` は `AuthenticatedPeer` のときだけ true になる。
- `PeerCandidateFound`（candidate は有効だが未認証）や `ReadyNoPeer` は `peer_present=false`。
- `wrong_route_or_candidate_never_becomes_peer_present` test が、`route_scoped=false` の
  candidate が `authenticated=true` でも `ReadyNoPeer` / `peer_present=false` に留まることを
  固定している。
- **現行の課題**: `PeerObservation.authenticated` は observation 側に埋め込まれており、
  実 production の HMAC control 認証とは接続されていない。N-02 で実 HMAC handshake の
  結果を `authenticated` へ接続する。
- ICMP echo は discovery にも authentication の根拠にもしない（spec §9.3・§13.3）。

## 6. network event と periodic snapshot の競合

`network_events.rs` の `spawn_network_event_monitor` を正とする。network event は
**ヒント**であり、mode を直接変更しない。

| 入力 | 処理 | 出力 |
|---|---|---|
| 起動 | — | `RescanReason::Initial` で即時 rescan |
| Link / Ipv4 / Setup / InterfaceList event（generation 一致） | **debounce** 後にまとめる | `RescanReason::DebouncedNotification` で rescan |
| 古い generation の event | 無視 | なし |
| reconcile interval（30s、spec §13.5） | 定期 | `RescanReason::Reconcile` で rescan |

**優先順位**:

1. **新しい epoch の snapshot が常に勝つ**。stale snapshot は新しい snapshot で上書きされ、
   判定へ使わない。
2. **peer loss を最優先で検出する**。fresh snapshot が `AuthenticatedPeer` でなくなった場合、
   reconcile / promotion / backoff より先に single recovery owner へ収束させる（A-01〜A-04 の
   単一 owner 方針を維持）。
3. **debounce は burst を潰す**。複数 event が短時間に届いても rescan は一度だけ。
4. **event loss は 30s reconcile で回復する**。通知欠落があっても定期 rescan が snapshot を
   再取得する。

**失効条件**: 各 rescan は最新 epoch の snapshot を生成し、それ以前の snapshot は失効する。
`Bonjour candidate 消失のみ`では lease expiry まで current mode を維持する（spec §13.5）。

## 7. macOS API 失敗時の fail closed / 既存 lease 維持

macOS System Configuration / `getifaddrs` の失敗は **fail closed** とする。

| 観測 | `ThunderboltIpState` | 挙動 |
|---|---|---|
| network service なし | `ServiceMissing` | peer present にしない |
| service 無効 / IPv4 無効 | `ServiceDisabled` | peer present にしない |
| interface なし / 非 `bridge0` / down | `InterfaceUnavailable` | peer present にしない |
| address なし | `AddressMissing` | peer present にしない |
| address / prefix 競合 | `AddressConflict` | peer present にしない |
| 正常・candidate なし | `ReadyNoPeer` | peer present にしない |
| candidate 有効・未認証 | `PeerCandidateFound` | peer present にしない |
| candidate 有効・認証済み | `AuthenticatedPeer` | **peer present**、pairing 開始可能 |

**時系列の定義**:

1. **新しい lease は確立しない**。`route_scoped=false` 相当として扱い、
   `establish()` / `renew()` は `RouteNotScoped` で拒否する。
2. **promotion は開始しない**。`AuthenticatedPeer` でなければ pairing / promotion へ進まない。
3. **既存 lease は失効まで current mode を維持する**（spec §13.5 の「candidate 消失のみ」）。
   ただし address / route 消失または lease 失効は **future admission を閉じ**、
   `Solo Standalone` へ収束する（single recovery owner、A-01〜A-04）。
4. macOS API の一時失敗で即座に serving を落とさない。snapshot が `ServiceMissing` 等の
   unavailable を示しても、既存 lease の期限（15s）までは現 mode を継続し、lease 失効後に
   Solo へ収束する。

## 8. 真理値表

`attach / detach`、`wrong interface / subnet`、`stale candidate`、`Bonjour failure` を
固定する。`peer_present` 列は production の pairing 判定結果。

| シナリオ | observation / candidate | `ThunderboltIpState` / candidate 判定 | `peer_present` |
|---|---|---|---|
| attach（正常 pair） | service / interface / address 正常、candidate 有効、認証済み、lease 有効、stability 継続 | `AuthenticatedPeer` | **true** |
| detach（route 喪失） | `route_scoped_to_interface=false` または interface down | `ReadyNoPeer`（または `InterfaceUnavailable`）/ `RouteNotScoped` | **false** |
| wrong interface | `interface_index != expected` | `CandidateError::WrongInterface` | false |
| wrong subnet | address が期待 subnet 外 | `CandidateError::WrongSubnet` / `AddressConflict` | false |
| wrong address | address が期待 peer でない | `CandidateError::UnexpectedAddress` | false |
| stale candidate（旧 generation） | `generation != current` | `CandidateError::OldGeneration`（event も generation フィルタ） | false |
| Bonjour failure（NotPermitted 等） | static fallback 有効、route scoped、port 非 0 | `StaticFallback` candidate（要 HMAC 認証） | 認証まで false |
| Bonjour failure + static 無効 / route 非 scoped | `allows_static_fallback=false` または `route_scoped=false` | `StaticFallbackNotAllowed` / `RouteNotScoped` | false |
| macOS API 失敗 | service / interface 取得不能 | `ServiceMissing` 等 unavailable | **false**（fail closed、lease 失効まで現 mode 維持） |
| 認証未完了 | candidate 有効、`authenticated=false` | `PeerCandidateFound` | false |

## 9. N-02 での接続設計（`route_scoped=true` 置換）

N-01 の完了条件は「production handler が `route_scoped=true` を固定値で渡す必要がなくなる
設計が承認済み」である。N-02 で次の置換を行う。

- `ProductionClusterRuntime::handle()` の `.node_descriptor(&authenticated, true, now)` /
  `.handle(..., true, now)` の **4 箇所の固定 `true`** を、最新 epoch の
  `NetworkSnapshot` 由来の実測 `route_scoped` へ置換する。
- `route_scoped` の真偽は `NetworkSnapshot` の `candidate_valid`（
  `route_scoped_to_interface && candidate == expected_peer`）と、snapshot が
  `AuthenticatedPeer` であることを用いて決める。
- snapshot epoch と request 時点の整合を検証し、stale snapshot では lease
  establish / renew を許可しない。
- 検証済み snapshot / candidate を production runtime へ共有し、event loss は
  periodic reconcile で回復する。

停止条件（本設計が満たさなければ N-01 は承認できない）:

- ICMP や Bonjour presence だけを trust する必要がある。
- `route_scoped` を production で実測せず固定値のままにする。
- network 設定変更や shell output parsing を通常経路にする。

## 10. 完了条件 / operator review

- **完了条件**: production handler が `route_scoped=true` を固定値で渡す必要がなくなる
  設計が本稿で承認されたこと。本稿は N-01（Actor: agent + operator review）の agent
  成果物であり、**operator review を待って** `[x]` へ確定する。承認後、N-02 で
  `route_scoped` の実測化を実施する。
