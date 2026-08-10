# P7-02 migration and rollback rehearsal — 2026-08-11

## Result

Status: **PASS**

MacBook Proをrehearsal node、稼働中のMac Studioを代替nodeとして、legacy config拒否、旧SQLite不変、新旧config分離、release candidateへのupgrade、直前binaryへのrollback、candidateへの再upgradeを確認した。Model、KV cache、secret、cluster state、legacy SQLiteは削除していない。

## Artifacts

| Item | Value |
|---|---|
| Release candidate source | `phase/7-release` commit `19bc605` |
| Previous binary SHA-256 | `47d5488b6132703dd2144981917f13aa6b84790060ec5627eab2ab5ea476e841` |
| Candidate binary SHA-256 | `cb2ae7c4bb72cacfe23e90f99f9676d11fc8824161cc1f25f57d7ec9b31fbc17` |
| Rehearsal storage | user application-support directory内の`release-rehearsal-20260811`隔離path |
| Legacy config | repositoryの`tests/fixtures/legacy/siderostat.example.toml` |
| Mode-aware config | P4-08/P5-04で検証済みのnode別schema v2 config |

隔離pathにはprevious/candidate binaryを別名で保存した。LaunchAgent plist、model、secret、state、cacheのpathは変更していない。

## Migration checks

| Check | Result | Evidence |
|---|---|---|
| Legacy config rejection | PASS | Candidateはexit 1となり、`backends`、`routing`、`affinity`、`heartbeat`、`active_probe`、`cooldown`を列挙してschema v2の移行先を示した |
| Legacy SQLite unchanged | PASS | Rejection実行前後でSHA-256、size 16384 bytes、mtimeが一致 |
| Config separation | PASS | repository legacy fixtureと稼働用schema v2 configは異なるabsolute path |
| User data preservation | PASS | SQLite、model、KV、secret、stateを削除・移動・上書きしていない |

## Binary round trip

各段階でLaunchAgentを`kickstart -k`し、PID更新、proxy 1 process、proxy所有DS4 child 1 process、`/healthz`、`/readyz`を確認した。

| Stage | Binary | Result |
|---|---|---|
| Initial | previous | Solo Standalone ready |
| Upgrade | candidate | 約30秒でSolo Standalone readyへ復帰 |
| Rollback | previous | 約30秒でSolo Standalone readyへ復帰 |
| Re-upgrade | candidate | 約30秒でSolo Standalone readyへ復帰、`cluster doctor --json`の`healthy=true` |

最終状態はcandidate binary、SoloStandaloneReady、proxy 1、DS4 child 1である。Rollback artifactはrelease終了まで保持する。

## Recovery

再upgradeに失敗した場合は、隔離pathのprevious binaryをsymlink targetへatomic replacementし、標準LaunchAgentを`kickstart -k`する。Readiness確認前にpairing/promotionへ進まない。Config、state、model、KV、secret、legacy SQLiteはrollback操作の対象にしない。
