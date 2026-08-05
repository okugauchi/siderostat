# Contributing to DS4 Smart Proxy

## Git運用方針

本書はrepository全体で継続して適用するGit運用の正本である。特定releaseの作業順序や進捗は `docs/implementation-plan.md`、製品behaviorは `docs/spec.md` で管理する。

### 基本原則

- 設計変更を通常のcommit ancestryとして残し、既存履歴を書き換えない。
- `main` はrelease可能な状態を維持する。
- `main`、保存用branch、公開済みtagへforce pushしない。
- Branch名に `codex/` prefixを使用しない。
- Orphan branch、`git reset --hard`、履歴の強制置換を通常の移行手段として使用しない。
- Secretや配布不能artifactを履歴から除去する必要がある場合は、影響範囲と復旧方法を定めて別途承認を得る。

### Branchとtag

| Pattern/ref | 用途 |
|---|---|
| `main` | release可能な現行版 |
| `legacy/<architecture>-<version>` | 旧architectureの保存とhotfix起点 |
| `rewrite/<topic>` | 大規模刷新のintegration |
| `phase/<number>-<topic>` | 実装計画のphase単位 |
| `feature/<topic>` | 通常の独立機能 |
| `fix/<topic>` | 現行版の修正 |
| `hotfix/<release>/<topic>` | 過去releaseの限定修正 |

Branch名はlowercase ASCIIとhyphenを基本とし、目的が判別できる名前にする。個人名、agent名、曖昧な `work` や `changes` を恒久branch名に使用しない。

Releaseまたはrollback基準点にはannotated tagを使用する。公開済みtagを削除・付け替えしない。誤ったtagを公開した場合は新しい修正tagを作成する。

### Load balancerからmode-aware proxyへの移行

次のrefを移行完了後も保持する。

| Ref | Target/用途 |
|---|---|
| `legacy/load-balancer-v1` | Load balancer系最終版。初期targetはtest済みcommit `b66ba1c` |
| `load-balancer-v1-final` | 同commitを固定するannotated tag |
| `rewrite/mode-aware` | Mode-aware architecture実装中のintegration branch |

Legacy branchは保存専用とし、通常開発を継続しない。旧版の修正は `hotfix/load-balancer-v1/<topic>` から行い、新しいpatch tagを付ける。Legacy hotfixを現行architectureへ機械的にmergeせず、同じ問題が存在するか個別に評価する。

### Phase branch

```text
main
  `-- load-balancer-v1-final
       |-- legacy/load-balancer-v1
       `-- rewrite/mode-aware
            |-- phase/0-compatibility
            |-- phase/1-target-resolver
            |-- phase/2-thunderbolt-pairing
            |-- phase/3-process-supervisor
            |-- phase/4-distributed
            |-- phase/5-recovery-operations
            |-- phase/6-user-documentation
            `-- phase/7-release
```

- `phase/*` はintegration branchの最新accepted commitから作成する。
- Review先は `main` ではなく対象integration branchとする。
- Phase内で分割が必要な場合だけ、小さい `feature/*` または `fix/*` branchを作る。
- Phase完了時に試行錯誤commitを整理してよいが、integration branchにはreview可能な論理commitを残す。

### Commit

- 1 commitを1つのreview可能な目的に限定する。
- 実装と直接対応するtestを同じcommitへ含める。
- Formattingだけの大量差分をbehavior changeと混在させない。
- Generated artifact、model、secret、runtime stateをcommitしない。
- Commit subjectは変更結果を命令形で表す。
- 必要に応じて本文へ仕様書section、test evidence、migrationまたはrollbackへの影響を書く。

### Reviewとmerge

Reviewでは少なくとも次を確認する。

- Target behaviorと仕様書の整合。
- Failure behaviorとrollback可能性。
- Unit/integration/actual test evidence。
- Config、運用、README、導入ガイドへの影響。
- Secret、model、runtime artifactが含まれていないこと。

大規模刷新はintegration branch側でrelease gateを満たしてから `main` へmergeする。最終mergeでintegration branch全体を1 commitへsquashせず、phase単位の履歴を保持する。Merge後は `main` で全verification gateを再実行し、release tagとcompatibility recordを作成する。

### Branch protection

Repository hostで可能なら `main` と `legacy/*` に次を設定する。

- Direct push禁止または明示的review必須。
- Required CI成功後だけmerge可能。
- Force push禁止。
- Legacy branchの削除禁止。

Required CIの最低条件：

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

### Branchの終了

- Merge済みの `feature/*`、`fix/*`、`phase/*` はremote/localとも整理してよい。
- `rewrite/*` は最終merge後に追加開発へ再利用しない。
- `legacy/*` とrelease/rollback tagは保持する。
- Branch削除前にmerge済みであること、または必要commitが別refから到達可能であることを確認する。
