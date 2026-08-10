# P6-05 documentation installation verification: 2026-08-10

## Result

Status: **PASS (relaxed existing-environment acceptance)**

Repository内で実行できる文書・command・artifact検証と、P3-07/P4-08の既存2-node actual acceptance証跡を組み合わせ、導入手順と両roleの期待結果を確認した。現在利用中の環境を初期化できないため、clean user accountの2-node sequential runはacceptanceから除外した。このrecordは緩和後のPhase 6 exit conditionを満たす。

## Scope

| Item | Value |
|---|---|
| Repository commit | `f7e7d2e` |
| Verification host | Apple arm64 / macOS 26.6.1 (25G76) |
| Documents | `README.md`、`docs/installation.md`、`docs/spec.md`、`docs/operations.md`、`docs/troubleshooting.md`、`contrib/launchd/README.md` |
| Actual user environment | 現在利用中の既存user account |
| Nodes exercised | Repository-local gateは1 node。P4-08のexisting coordinator/worker evidenceを両role確認に再利用 |
| Runtime mutation | 本検証ではLaunchAgent、secret、model、config、runtime stateを変更せず |

## Repository-local evidence

| Check | Result | Evidence |
|---|---|---|
| Release build | PASS | `cargo build --release` |
| Format | PASS | `cargo fmt --check` |
| Lint | PASS | `cargo clippy --all-targets --all-features -- -D warnings` |
| Tests | PASS | `cargo test --all-targets`: 145 tests passed |
| Config example | PASS | `config::tests::parses_repository_schema_v2_example` |
| CLI synopsis | PASS | top-level、cluster、status、doctor、demote、fingerprintの`--help`を実binaryと突合 |
| Local Markdown links | PASS | 11 Markdown filesのrelative link 22件が実在 |
| LaunchAgent template | PASS | `plutil -lint contrib/launchd/local.siderostat.runtime.plist` |
| LaunchAgent path replacement | PASS after correction | Spaceを含むtemporary pathで`PlistBuddy` replacement、placeholder不在、`plutil -lint` |
| Whitespace errors | PASS | `git diff --check` |

Test countはlibrary unit 142、phase2 integration 1、phase5 failure integration 1、phase5 security integration 1の合計145である。

## Findings corrected by this verification

1. READMEの`[--json]`、`[--reason TEXT]`、`standalone|distributed`を、そのままcopy/pasteできる個別commandへ変更した。
2. Control secretとpeer proxy tokenを両nodeで別々に生成するとHMAC control / peer ingress authenticationが成立しない。両nodeで各値を共有し、admin tokenだけをnode-localにする手順へ修正した。
3. `/usr/local/bin`へ直接`cp`する手順を、modeを固定した`sudo install`へ変更した。
4. LaunchAgent exampleを`USERNAME`のままbootstrapする手順を修正した。初回修正案の`plutil -replace ProgramArguments.3`はarray要素を追加して古いplaceholderを残したため不採用とし、実測で置換できた`/usr/libexec/PlistBuddy` を使用した。
5. Standalone、Paired Standalone、Distributed MXFP4の各段階で確認するmode/state/readyとadmin endpointの期待結果を導入ガイドへ追加した。

## Role checklist and evidence mapping

| Stage | Coordinator expected | Worker expected |
|---|---|---|
| Fixed address | `bridge0=10.99.0.1`、role=coordinator | `bridge0=10.99.0.2`、role=worker |
| Solo Standalone | mode/state=`solo-standalone` / `solo-standalone-ready`、target=`local-standalone`、ready=true | 同じ |
| Paired Standalone | mode/state=`paired-standalone` / `paired-standalone-ready`、target=`local-standalone`、ready=true | mode/state=`paired-standalone` / `paired-standalone-ready`、target=`coordinator`、ready=true |
| Distributed MXFP4 | mode/state=`distributed-mxfp4` / `distributed-ready`、ready=true | mode/state=`distributed-mxfp4` / `distributed-ready`、target=`coordinator`、ready=true |
| Existing service | P4-08でGUI user LaunchAgent起動、10回promotion/demotion、final readyを確認 | 同じ |

Actual evidenceにはcommandごとにtimestamp、exit status、上記のsanitized stateだけを記録する。User path、model path、prompt/body、secret/token、full model digest/deployment IDは本recordに保存しない。

## Non-blocking release follow-up

- Expected baseline `b7e9f00`のfull source commitは未確認。Actual distributed acceptanceはsource commit `b0309611041655f4e45671cfd9c9886aff161406`であり、P7 final release acceptanceでbaselineを確定する。
- Q2 residentは任意profileで、対応full standalone GGUF未配置はP6 / release blockerにしない。
- Login start、LaunchAgent restart、physical cable detach/reconnectの再実行は、必要に応じてoperatorが既存環境を保全した上で個別に行う。

上記follow-upはP6-05の完了を妨げない。Clean user accountへの環境初期化を行わず、repository-local gateと既存actual evidenceによりPhase 6 exit conditionをPASSとする。
