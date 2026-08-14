# reconnect 診断 contract

Status: **DRAFT**（operator review 待ち）

最終更新日: 2026-08-14

## 1. 目的と範囲

本稿は [`reconnect-improvement-proposal.md`](reconnect-improvement-proposal.md) 第 4 節 P0-0
の観測項目を、外部から read-only で採取できるようにするための field 定義である。対象は次。

- admin `/cluster` 応答（`cluster status --json` が同一 schema を使用）。
- control 応答（`/v1/node`、Pair の 409 を含む control error）。
- cluster transition / control の構造化 log。

本稿は R0-03 の実装契約であり、field 名・型・null の扱い・redaction を一意に定める。実装者は
本稿にない判断を推測せず、矛盾があれば正本（`spec.md`、proposal）を確認して本稿を更新する。

## 2. backward compatibility 方針

- 既存の `/cluster` field（`generation`、`role`、`mode`、`state`、`target`）は名前・型・意味を
  変更しない。
- 新規 field は**追加のみ**とし、既存 field の削除・rename・意味変更を行わない。
- JSON key は control protocol の serde 慣例に合わせて **kebab-case** を新規 field に使う。
  既存の単語 field（`generation` など）はそのまま維持する。
- 未知 field を受信側が拒否しない（`deny_unknown_fields` を診断 field には適用しない）。
- `null` は「情報が存在しない（未確定・未取得）」を意味し、偽の 0 値や空文字で代替しない。

## 3. `/cluster` 診断 schema

admin `GET /cluster` と `cluster status --json` は次の JSON を返す。`children` はローカル
runtime の child を表し、`control_session` はローカル control processor の状態を表す。

```json
{
  "generation": 12,
  "cluster_generation": 12,
  "role": "coordinator",
  "mode": "distributed-mxfp4",
  "state": "distributed-ready",
  "target": "local-standalone",
  "target_ready": true,
  "admission": "open",
  "control_session": {
    "generation": 7,
    "phase": "paired",
    "role": "coordinator",
    "lease": {
      "valid": true,
      "expires-at-millis": 1770000000000,
      "route-scoped": true,
      "peer-present": true,
      "peer": {
        "node-id": "worker-node",
        "role": "worker",
        "generation": 7,
        "mode": "distributed-mxfp4"
      }
    }
  },
  "children": {
    "standalone": {
      "pid": 1234,
      "profile": "standalone",
      "generation": 12,
      "running": true,
      "ready": true
    },
    "distributed-coordinator": {
      "pid": 5678,
      "profile": "distributed",
      "generation": 12,
      "running": true
    },
    "distributed-worker": {
      "pid": null,
      "profile": null,
      "generation": null,
      "running": false
    }
  }
}
```

### 3.1 top-level field

| field | 型 | 意味 |
|---|---|---|
| `generation` | u64 | （既存）cluster generation。後方互換のため維持 |
| `cluster_generation` | u64 | cluster generation の明示名。`generation` と同値 |
| `role` | enum | （既存）coordinator / worker / unknown |
| `mode` | enum | （既存）stable mode |
| `state` | enum | （既存）cluster state |
| `target` | enum | （既存）proxy target |
| `target_ready` | bool | target が ready かつ admission 可能か |
| `admission` | enum | `open` / `blocked`。将来の request 受付可否 |

`role` が unknown、または runtime 未確定の場合は `mode`/`state`/`target` と同様に現行の
値を使い、`control_session` と `children` は該当する情報が無い限り `null` にする。

### 3.2 control_session

`control_session` はローカル control processor（`CoordinatorControl` または `WorkerControl`）の
read-only snapshot。情報が未構築（runtime 未確定）のときは `null`。

| field | 型 | 意味 |
|---|---|---|
| `generation` | u64 | control session generation（ControlMessage.generation）。peer から受け取った最新の確定 session 世代 |
| `phase` | enum | DistributedControlPhase: unpaired / paired / worker-preparing / worker-ready / draining / drained |
| `role` | enum | ローカル control role: coordinator / worker |
| `lease` | object | 下記の lease snapshot |

### 3.3 control_session.lease

peer lease の状態。lease 未確立なら `valid=false`、`expires-at-millis=null`、
`peer-present=false`、`peer=null`。

| field | 型 | 意味 |
|---|---|---|
| `valid` | bool | 期限が未来かつ未失効か（`expires-at-millis` が future） |
| `expires-at-millis` | u64 \| null | lease 期限 (UNIX epoch ms)。未確立なら null |
| `route-scoped` | bool | route が cluster interface に scoped されているか |
| `peer-present` | bool | `route-scoped && stable && !expired && descriptor あり` の合成判定 |
| `peer` | object \| null | peer descriptor。未確立なら null |

### 3.4 control_session.lease.peer

| field | 型 | 意味 |
|---|---|---|
| `node-id` | string | peer の node_id。secret ではない安定識別子 |
| `role` | enum | peer control role: coordinator / worker |
| `generation` | u64 | peer descriptor の generation |
| `mode` | enum | peer の stable mode |

完全な `deployment_id` は redaction 対象のため含めない（§7）。

### 3.5 children

`children` はローカル child supervisor が管理する standalone / distributed child の identity。
該当 role に存在しない supervisor（例: worker の `distributed-coordinator`）は `null` にする。
child が未起動なら各 field を `null`、`running=false` にする。

| field | 型 | 意味 |
|---|---|---|
| `standalone` | child \| null | ローカル standalone child |
| `distributed-coordinator` | child \| null | coordinator role の distributed child（worker では null） |
| `distributed-worker` | child \| null | worker role の distributed child（coordinator では null） |

### 3.6 child identity

各 child は次の shape。

```json
{
  "pid": 1234,
  "profile": "standalone",
  "generation": 12,
  "running": true,
  "ready": true
}
```

| field | 型 | 意味 |
|---|---|---|
| `pid` | u32 \| null | 生存中 child の pid。未起動・未確認なら null |
| `profile` | string \| null | profile_id。未確定なら null |
| `generation` | u64 \| null | child が起動された generation。未確定なら null |
| `running` | bool | supervisor が child 生存を確認しているか |
| `ready` | bool | standalone のみ。readiness 確認済みか。distributed child では省略または false |

`argv_sha256`、`executable`、`spawned_at_millis`、`process_start_micros` などの内部 identity は
診断 schema に含めない（高 cardinality・内部実装依存のため）。

## 4. transition log event

cluster transition と control の開始・完了・失敗は、次の共通 field を持つ構造化 log として
emit する。`event` と `owner` は必須。情報が無い field は省略する。

| field | 型 | 意味 |
|---|---|---|
| `event` | string | イベント名（下記 §5 を参照） |
| `owner` | enum | 発火元: `periodic-reconcile` / `route-loss-monitor` / `admin` / `control` / `promotion` / `recovery` |
| `from` / `to` | enum | cluster state 遷移。該当する場合のみ |
| `cluster_generation` | u64 | 遷移後の cluster generation |
| `control_session_generation` | u64 | 関連する control session generation（該当時のみ） |
| `result` | enum | `success` / `rejected` / `failed` |
| `reason` | string | ClusterEventKind または ClusterFailure の判別名 |

現行 `spawn_state_machine` の transition log は `event/from/to/reason/result/generation` を持つ。
本 contract はこれに `owner` と `control_session_generation` を追加する。既存 field は維持する
（backward compatibility）。

## 5. イベント名一覧

| event | owner | 発火条件 |
|---|---|---|
| `peer-lost` | periodic-reconcile / route-loss-monitor | PeerLost 検出。Solo recovery 開始 |
| `recovery-started` | recovery | PeerLost 後の recovery owner 取得・admission block |
| `recovery-completed` | recovery | SoloStandaloneReady + target=LocalStandalone publish |
| `recovery-failed` | recovery | stop/start/readiness/identity のいずれか失敗 |
| `pairing-started` | periodic-reconcile / admin | BeginPairing |
| `pairing-ready` | control / periodic-reconcile | PairingReady 到達 |
| `promotion-started` | periodic-reconcile / admin | BeginPromotion |
| `promotion-failed` | promotion | PromotionFailed。`reason` に ClusterFailure |
| `demotion-started` | route-loss-monitor / admin | BeginDemotion |
| `pair-generation-mismatch` | control | Pair が GenerationMismatch で 409。`expected`/`received` を記録（§6） |
| `cluster-transition-rejected` | 各 | 状態機械が stale generation / invalid transition で reject |

## 6. Pair 409 log

Pair が `ControlError::GenerationMismatch` で拒否されたとき、次の field を構造化 log に
含める。`event=pair-generation-mismatch`、`owner=control`。

```json
{
  "event": "pair-generation-mismatch",
  "owner": "control",
  "result": "rejected",
  "expected": 9,
  "received": 5,
  "cluster_generation": 12,
  "control_session_generation": 9
}
```

| field | 型 | 意味 |
|---|---|---|
| `expected` | u64 | 受信側の現在 generation（拒否側の session generation） |
| `received` | u64 | Pair message に含まれた generation（提案側） |
| `cluster_generation` | u64 | 受信側の cluster generation |
| `control_session_generation` | u64 | `expected` と同値 |

**redaction**: secret、signature、nonce、完全な deployment_id、request_id を 409 log に
含めない。`expected`/`received` は generation 値のみ。

## 7. redaction 規則

- 含めない: secret、HMAC signature、nonce、完全な `deployment_id`、model path、GGUF、KV cache
  path、request body。
- `node_id` は secret ではない安定識別子として含めてよいが、metrics label には使わない
  （高 cardinality 回避）。
- `pid`、`profile_id`、generation は含めてよい。
- 完全な deployment_id が必要な比較は、内部処理に限定し、log/JSON へは出力しない。
- 実機 evidence は repository 外に採取し、redaction 済み要約のみ repository へ入れる。

## 8. proposal P0-0 との対応表

| proposal P0-0 観測項目 | 本 contract の field / log |
|---|---|
| 両 node の cluster generation | `cluster_generation` |
| 両 node の control generation | `control_session.generation` |
| Pair 409 の expected / received generation | §6 `expected` / `received` |
| state | `state` |
| stable mode | `mode` |
| proxy target | `target`（+ `target_ready`） |
| admission | `admission` |
| standalone / distributed child の PID | `children.*.pid` |
| standalone / distributed child の profile | `children.*.profile` |
| standalone / distributed child の generation | `children.*.generation` |
| standalone / distributed child の生存状態 | `children.*.running`（standalone は `ready`） |
| PeerLost / route-loss demotion / Pair / promotion の開始・完了順序 | §5 transition log（`event`/`owner`/`from`/`to`/`result`） |
| lease 有効性と期限 | `control_session.lease.valid` / `expires-at-millis` |
| route scope | `control_session.lease.route-scoped` |

## 9. 実装上の注意（R0-03 への引き継ぎ）

- `/cluster` と `cluster status --json` は同一の serializer を使う。
- control session / lease / child identity は production runtime の read-only snapshot として
  取得し、mutation endpoint を追加しない。
- `control_phase` は `DistributedControlPhase` の enum 名を kebab-case で出力する。
- child の `running` は supervisor の現在の生存確認に基づく。確認に失敗した場合は `null` でなく
  `false` とし、`pid` を `null` にしない（最後に確認できた identity を維持する）。

## 10. 導出元の確定（operator 承認を依頼）

本稿は次の導出を code の evidence に基づき確定提案する。operator が承認をもって確定する。

1. **`admission`**: `AdmissionGate::snapshot().state` が `Serving` なら `open`、それ以外
   （`Draining` / `Blocked`）は `blocked` とする。evidence: [`src/admission.rs`](../src/admission.rs) の
   `AdmissionState { Serving, Draining, Blocked }`。permit 残量（in_flight/max）は `open`/`blocked`
   の判定に使わない。容量は `try_acquire` が別途制御するため。
2. **`target_ready`**: `admission == open` かつ `ProxyTarget` が `LocalStandalone` または
   `Coordinator`（`Unavailable` でない）のときに `true`。evidence: [`src/target.rs`](../src/target.rs) の
   `ProxyTarget { LocalStandalone, Coordinator, Unavailable }`。`target` と独立に同じ source から導出する。
3. **distributed child の `ready`**: distributed child は supervisor の identity に readiness 概念が
   ないため `ready` field を常に省略する（standalone のみ `ready` を持つ）。
