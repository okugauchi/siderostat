# Reconnect実機 acceptance — 2026-08-17

## 判定

**PASS — candidate 継続利用を承認。**

operator 判断により rollback は実施しない。検証済み candidate は、後続の
`feature/reconnect-recovery` から `develop` への merge、binary/config の正規名称への
切り替え、release candidate artifact の再生成・checksum 確定へ進める。rollback 用の
binary/config/state/plist は削除せず、緊急時の復旧手段として保持する。

この文書は release tag や `develop` merge の完了を意味しない。Release Candidate としての
最終確定は、merge 後に正規名称で再生成した artifact の checksum を確定した時点とする。

## 対象 build と保全

| 項目 | 内容 |
|---|---|
| verification source implementation | commit `05220cb`（H-03 reconnect recovery / owned child cleanup） |
| documentation/current branch | `feature/reconnect-recovery`、commit `5717922` |
| candidate binary | `$HOME/Library/Application Support/siderostat/candidate-reconnect-20260816-auto-repair-sigkill/siderostat` |
| candidate binary SHA-256 | `1952cd7bb2db07ffcb4e5487fed3a52949f3d2d7f85b4c35a57f7391fc4f3cb0`（両 node 共通） |
| worker config SHA-256 | `74032583b901aa67c02d3c3e51c2446e089971ee427f3dbbc640970fada53daa` |
| coordinator config SHA-256 | `e480260f3c352e227d1f71014877c7766b562c7eee794606619ad85e4d0d0a78` |
| LaunchAgent plist SHA-256 | `ed090fe6aaa406bc09f15866a8c70880dff9f3cf342f06ff8289fbd30f2f6586`（両 node 共通） |
| runtime policy | `stop=180s`、`allow_sigkill=true`。SIGKILL は identity-confirmed owned child に限定 |
| rollback保全 | worker/coordinator の rollback directory と H-01 backup を保持。削除・置換なし |

## 実機シナリオ結果

| task / scenario | 操作・結果 | 観測された最終状態 |
|---|---|---|
| H-01 | candidate 配置、LaunchAgent 起動、startup cleanup、rollback backup を確認 | 両 node Solo Standalone ready、doctor healthy、child 各1件 |
| H-02 | Thunderbolt cable detach/reconnect を2 cycle | 各 cycle で Solo Standalone → Paired → DistributedReady。stale DS4 cleanup は各約3〜4分、orphanなし |
| H-03 初回 | coordinator-only restart 後、SIGKILL拒否と `/v1/node` 409 loopを検出して停止 | **FAIL evidence** として保存。修正・candidate再適用後に再検証 |
| H-03 修正後 | coordinator-only 2 cycle、worker-only 2 cycle | 各 cycle で自動 recovery / re-pair / promotion / DistributedReady。新 child PID/generation、409 loop・古い child再利用なし |
| H-04 coordinator-only | Mac Studio reboot、MacBook Pro は稼働継続 | boot `08:50:47`、約3分後に両 node DistributedReady、API 200 |
| H-04 worker-only | MacBook Pro reboot、Mac Studio は稼働継続 | boot `08:57:04`、約2分後に両 node DistributedReady、API 200 |
| H-04 both | MacBook Pro / Mac Studio をほぼ同時 reboot | boot worker `09:03:42`、coordinator `09:03:40`。promotion中の一時 blocked/503後、約3分で両 node DistributedReady |

H-03 初回失敗、H-02 の並列 smoke における `max_in_flight=1` 起因の一時 HTTP 503、H-04
両 node reboot 中の promotion transition 503 は、最終 PASS とは別に記録済みである。成功
cycle の最終 checkpoint では、manual pair/reconcile、state 削除、force kill は実施していない。

## Proposal 第1節 acceptance criteria との対応

| criterion | 判定 | 根拠 |
|---|---|---|
| 1. peer loss後に両 node が Solo Standalone serving | PASS | H-02/H-03/H-04 の recovery checkpoint、doctor/admission/standalone readiness |
| 2. stale distributed child / orphan transition が残らない | PASS | 各 final process tree、DS4 child 各 node 最大1件、H-03/H-04 log確認 |
| 3. 再接続後に Paired Standalone へ収束 | PASS | H-02/H-03/H-04 の pairing-ready checkpoint、lease valid / peer-present |
| 4. auto promotionで DistributedReady へ復帰 | PASS | H-02 2 cycle、H-03 4 cycle、H-04 3 case の distributed route ready |
| 5. state / proxy target / admission / child identity が一致 | PASS | 各 final `cluster status` / `cluster doctor`、public `/v1/models` HTTP 200、PID/generation確認 |

## Final checkpoint

両 node 同時 reboot の最終値は次のとおり。

- worker: `cluster_generation=452`、distributed worker PID `2578` / generation `449`
- coordinator: `cluster_generation=609`、distributed coordinator PID `1068` / generation `607`
- control session generation: 両 node `601`
- 両 node: `healthy=true`、admission `serving`、in-flight `0`
- lease: `valid=true`、`peer-present=true`、`route-scoped=true`
- public `/v1/models`: worker/coordinator とも HTTP `200`
- LaunchAgent: `local.siderostat.runtime` 各1件、duplicate jobなし
- boot後 log window: 409 loop、`SIGKILL is not allowed for this child`、orphan childなし

## Evidence

- H-01/H-02 baseline・rollback・cable artifacts: `$HOME/siderostat-reconnect-evidence-20260815/`
- H-03 completion record: `docs/archive/reconnect-2026-08/reconnect-field-verification-runbook.md` §8.3、
  `docs/archive/reconnect-2026-08/reconnect-improvement-implementation-plan.md` H-03 completion evidence
- H-04 raw summary: `/private/tmp/siderostat-reconnect-evidence-20260817/h04/20260817-h04-summary.md`
- H-04 runbook record: `docs/archive/reconnect-2026-08/reconnect-field-verification-runbook.md` §9.1

H-03 の一時 evidence directory は検証環境の temporary storage であり現行環境には残って
いないため、H-03 の acceptance は repository に commit 済みの runbook/plan completion
record と、実機ログ・最終 checkpoint の記録に基づく。未記録の成功を推測していない。
