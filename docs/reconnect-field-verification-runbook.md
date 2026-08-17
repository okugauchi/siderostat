# 実機検証（Phase H）Runbook

本稿は `docs/reconnect-improvement-implementation-plan.md` の Phase H（H-01〜H-05）を、2 node 実機で
実施するための operator 向け手順・報告・証跡の説明資料である。operator（ユーザー）が行う物理操作・
macOS 操作のガイダンス、operator から agent への報告方法、証跡の残し方を定める。

## 1. 目的とスコープ

- 対象: Thunderbolt 固定 IPv4 で接続した coordinator / worker の 2 node。
- 検証する reconnect 動作（`docs/reconnect-improvement-proposal.md` 第1節の 5 条件に対応）:
  - cable detach/reconnect（H-02）
  - 片側 process 再起動（H-03）
  - macOS 再起動 片側・両側（H-04）
- 本稿で agent は command 提案と read-only 観測を担当し、operator は物理 cable / macOS / service
  操作と candidate binary 配置の承認を担当する。

## 2. 役割分担

| 担当 | 内容 |
|---|---|
| operator（ユーザー） | cable 抜き差し、macOS 再起動、`launchctl` 操作の承認と実行、candidate binary 配置の承認、change window の管理 |
| agent | 観測・結果整理・command 提案・証跡の集約・redaction 済み acceptance 文書の作成 |

agent は operator の明示的依頼なしに実機 service の停止・再起動・binary 上書き・macOS 再起動を行わない。

## 3. 前提条件（H-01 着手条件）

次のすべてを満たしてから開始する。

- 共通 local gate と reconnect acceptance suite が GREEN（agent が最終確認する）。
- 両 node が同じ candidate build で稼働している。
- change window 中に利用停止が可能で、進行中 inference request がない。
- 両 node の時計が大きくずれていない（`max_clock_skew` 既定 30s 以内）。

### 3.1 現在稼働中の構成（＝バックアップ・rollback 対象）を記録する

> **重要な区別**: 実機検証では「**実験対象（candidate）**」と「**現在稼働中（現行）**」を明確に分ける。
> - **現在稼働中（現行）** = 検証前に `/usr/local/bin/siderostat` に導入され稼働している構成。
>   H-01 step 3 で **削除せず backup** し、問題発生時に **rollback 先** になるもの。
> - **candidate（実験対象）** = これから検証する新 build。**まだ導入していない**。H-01 step 2 で
>   agent が commit SHA / binary SHA-256 / config checksum を記録し、operator の承認後に導入する。
>
> 本節 3.1 が記録するのは **現在稼働中（backup / rollback 対象）** の構成情報である。
> candidate は **§3.2** に別途記録する。

「記録欄」には、**既定パスや例示値ではなく、実機で実際に確認できた値を node 別に書く**。
表の「場所（既定）」はあくまで想定値で、実機の config や LaunchAgent 設定が異なる場合がある。
この表は H-01 の baseline 記録・rollback 手順・証跡のファイル名に使うため、**coordinator と worker の
2 台それぞれで**確認して埋める。確認は各 node の terminal で実行する。

**確認手順（各 node で実行）**:

```sh
# 1) config の実パスを確認し、中身を読む（plist 形式でない TOML のため sed で読むのが確実）
CONFIG="$HOME/Library/Application Support/siderostat/config.toml"
ls -l "$CONFIG"
sed -n '1,200p' "$CONFIG"

# 2) 実装済み binary のパスと SHA-256（rollback 時の比較対象）
ls -l /usr/local/bin/siderostat
shasum -a 256 /usr/local/bin/siderostat

# 3) LaunchAgent plist の実パスと label、ログ出力先を確認
PLIST="$HOME/Library/LaunchAgents/local.siderostat.runtime.plist"
ls -l "$PLIST"
/usr/libexec/PlistBuddy -c "Print :Label" "$PLIST"
/usr/libexec/PlistBuddy -c "Print :StandardOutPath" "$PLIST"
/usr/libexec/PlistBuddy -c "Print :StandardErrorPath" "$PLIST"

# 4) admin API / control port / ds4 distributed port は config の該当キーを読む
#    （例: [proxy] admin_listen、[cluster] control_port / ds4_distributed_port）
# 5) bridge0 の実 IP を確認（coordinator=10.99.0.1 / worker=10.99.0.2 の想定）
ifconfig bridge0
```

| 項目 | 場所（既定） | 記録内容 | coordinator 記録 | worker 記録 |
|---|---|---|---|---|
| binary | `/usr/local/bin/siderostat` | 実パス + SHA-256 | `o@m4max-macstudio.local:/usr/local/bin/siderostat` / `a1ee8cb4a51cd7a8fe77d76c2e0da523b5bc54205e4479800c998323a516abde` | `/usr/local/bin/siderostat` / `21c855ec5831c6fad6f5a821323a518492662b87f30d1878723c8da00125c0cb` |
| config | `$HOME/Library/Application Support/siderostat/config.toml` | 実パス（`sed` で確認） |  `o@m4max-macstudio.local:/Users/o/Library/Application Support/siderostat/config.toml` / `233cca9eab61faeb0c6e5544115ff7cf9675547cd2e2be1070f489085a23aa02` 　| `/Users/o/siderostat_backup/Users/o/Library/Application Support/siderostat/config.toml` / `a94e778092470cb2fefd7ad91f76e0e6378b3a1f663ec2c40418d008f6444912` |
| secret dir | `$HOME/Library/Application Support/siderostat/secrets` | 実パス | `o@m4max-macstudio.local:/Users/o/Library/Application Support/siderostat/secrets` | `/Users/o/Library/Application Support/siderostat/secrets` |
| state file | config `cluster.state_path` | 実パス | `o@m4max-macstudio.local:/Users/o/Library/Application Support/siderostat/cluster-state.json` | `/Users/o/Library/Application Support/siderostat/cluster-state.json` |
| LaunchAgent plist | `$HOME/Library/LaunchAgents/local.siderostat.runtime.plist` | 実パス | `o@m4max-macstudio.local:/Users/o/Library/LaunchAgents/local.siderostat.runtime.plist` | `/Users/o/Library/LaunchAgents/local.siderostat.runtime.plist` |
| LaunchAgent label | `gui/$(id -u)/local.siderostat.runtime` | plist の `Label` | `gui/$(id -u)/local.siderostat.runtime` | `gui/$(id -u)/local.siderostat.runtime` |
| 統合ログ | `$HOME/Library/Logs/siderostat/ds4-siderostat.log`（stdout/stderr を単一ファイルに統合） | plist の `StandardOutPath`/`StandardErrorPath` が一致しているか | (旧仕様から変更されていないため分割出力) `o@m4max-macstudio.local:/Users/o/Library/Logs/siderostat/stdout.log`, `o@m4max-macstudio.local:/Users/o/Library/Logs/siderostat/stderr.log` | `/Users/o/Library/Logs/siderostat/ds4-siderostat.log` |
| admin API | config `[proxy] admin_listen`（例 `127.0.0.1:18081`） | 実値 | `https://admin.siderostat.m4max-macstudio.home.arpa/` (`127.0.0.1:18081`) | `127.0.0.1:18081` |
| `bridge0` IP | coordinator `10.99.0.1` / worker `10.99.0.2` | `ifconfig bridge0` の inet | `inet 10.99.0.1 netmask 0xfffffffc broadcast 10.99.0.3` | `inet 10.99.0.2 netmask 0xfffffffc broadcast 10.99.0.3` |
| control port | config `[cluster] control_port`（例 `9920`） | 実値 | `9920` | `9920` |
| ds4 distributed port | config `[cluster] ds4_distributed_port`（例 `9911`） | 実値 | `9911` | `9911` |

**記入例（coordinator の場合）**:

| 項目 | 記録内容 | coordinator 記録 |
|---|---|---|
| binary | 実パス + SHA-256 | `/usr/local/bin/siderostat` / `9f2d…（16 桁以上）` |
| config | 実パス | `/Users/macstudio/Library/Application Support/siderostat/config.toml` |
| admin API | 実値 | `127.0.0.1:18081` |
| `bridge0` IP | `ifconfig` の inet | `10.99.0.1` |
| control port | 実値 | `9920` |
| ds4 distributed port | 実値 | `9911` |

> **注意**: 記録するのは secret 値・token そのものではなく、パスと IP・ポート・SHA-256 などの
> 構成情報のみ。secret / token は決して記録欄や証跡に書かない（§4.2 redaction）。

### 3.2 candidate（実験対象）の情報を記録する

3.1 の「現在稼働中」とは別に、**これから検証する candidate（実験対象）** をここに記録する。
candidate はまだ導入しておらず、H-01 step 2 で agent が記録し、operator の承認後にのみ導入する。
**導入前**に、導入予定の candidate binary そのもの（実機に置いたファイル）から SHA-256 を取る。
config は通常、現在稼働中のものを流用するため checksum は現行 config と一致するが、candidate 専用の
config を使う場合はその config を別途記録する。commit SHA は agent が repository から提供する。

| 項目 | 記録内容 | coordinator candidate | worker candidate |
|---|---|---|---|
| commit SHA | agent が repository から提供（`git rev-parse HEAD` 等） | `6bde9c10c148b3a85da6c015a5951bf21f3e898e` | `6bde9c10c148b3a85da6c015a5951bf21f3e898e` |
| candidate binary パス | 実際に LaunchAgent が起動する candidate の実パス | `/Users/o/Library/Application Support/siderostat/candidate-reconnect-20260816/siderostat`（build source: `/Users/o/Projects/github/okugauchi/siderostat/target/release/siderostat`） | `/Users/o/Library/Application Support/siderostat/candidate-reconnect-20260816/siderostat`（build source: `/Users/o/Projects/github/okugauchi/siderostat/target/release/siderostat`） |
| candidate binary SHA-256 | 導入予定ファイルの `shasum -a 256`（導入前に取得） | `c21bd1934cb531f5f1abd729429c3800492004c7067a2a20f5d2e6a2542a66a0` | `c21bd1934cb531f5f1abd729429c3800492004c7067a2a20f5d2e6a2542a66a0` |
| config checksum | 現行 config を使う場合は 3.1 と同一。candidate 専用ならその config の checksum | `233cca9eab61faeb0c6e5544115ff7cf9675547cd2e2be1070f489085a23aa02` | `a94e778092470cb2fefd7ad91f76e0e6378b3a1f663ec2c40418d008f6444912` |

```sh
# candidate binary の SHA-256 を導入前に取得（例）
shasum -a 256 /tmp/siderostat-candidate
# config checksum（現行 config を流用する場合）
shasum -a 256 "$HOME/Library/Application Support/siderostat/config.toml"
```

> **導入の流れ（H-01 step 2〜3 に対応）**:
> 1. agent が candidate の commit SHA / binary SHA-256 / config checksum をここに記録する。
> 2. operator が現行 binary / config / state / log を backup する（§6 step 3）。
> 3. operator の承認後にのみ、candidate binary を `/usr/local/bin/siderostat` へ配置する。
> 4. 問題が起きたら backup した現行 binary へ戻す（rollback）。

**2026-08-15 read-only 再確認**: `launchctl print` の実際の stdout/stderr は両 node とも
`/Users/o/Library/Logs/local.siderostat.runtime/ds4-server_siderostat.log` へ統合されていた。
3.1 表に記録された旧 worker の `ds4-siderostat.log` および coordinator の分割ログとは異なるため、
導入前 baseline の log artifact には `launchctl` の実測値を使用する。coordinator は
`solo-standalone-ready`（generation 330、admission serving）だったが、worker は admin API が
接続拒否で、LaunchAgent の再起動後も standalone DS4 child が HTTP readiness 前に exit status 2 で終了していた。
この状態では DistributedReady を baseline とみなさない。

**2026-08-15 candidate 配置結果**: `/usr/local/bin/siderostat` は root 所有で非対話 sudo が利用できなかったため、
現行 binary は上書きせず、保全済み plist の `ProgramArguments[0]` を上記 user-owned candidate path に変更した。
両 node とも plist lint と candidate SHA-256 は PASS。coordinator は candidate で
`solo-standalone-ready`（generation 332、health/ready PASS）へ復帰したが、worker は DS4 child が
HTTP readiness 前に exit status 2 で終了し続けたため LaunchAgent を bootout して再試行を停止した。
worker の plist は保全 backup と SHA-256 が一致する `/usr/local/bin/siderostat` 指定へ戻し、candidate は staged path に保持している。
現行 binary/config/state/plist/log の rollback backup は worker の
`/private/tmp/siderostat-reconnect-evidence-20260815/rollback/` と coordinator の
`/Users/o/siderostat-reconnect-evidence-20260815/rollback/` に保持している。

**2026-08-16 worker 起動失敗の原因調査**: worker の exit status 2 は DS4 instance lock 競合と判定した。
保存ログでは、distributed worker PID 1482 が `2026-08-15T09:46:39Z` まで稼働ログを出した後も残留し、
`2026-08-15T13:03:03Z` に restart reconcile が `PersistedChildStopFailed` で
`ManualInterventionRequired` へ遷移している。その後 `2026-08-15T14:15:38Z` からの standalone child は、
全て HTTP readiness 前に exit status 2 で終了した。DS4 source の `ds4_acquire_instance_lock()` は
`/tmp/ds4.lock` 競合時に exit 2 を返す。coordinator の controlled probe では実際に
`another ds4 process is already running (pid 98130); refusing to start` を再現し、worker の同一 standalone argv を
モデル `/dev/null` に置換した probe は CLI parse を通過してモデル形式エラー exit 1 になったため、argv 不整合ではない。
`allow_sigkill = false` と DS4 worker の SIGTERM 停止制約の組合せにより、残留 distributed child が lock を保持したまま
standalone 起動を妨げた。これは service の修正や live process の強制終了をまだ行わない調査結果である。

**互換性上の別問題**: 現在の DS4 checkout は `84cc882...` で、互換性記録 `docs/compatibility/ds4-b030961.md` の承認済み
`b030961...` と異なる。binary digest も worker `344006...` / coordinator `982011...` で、承認済み
worker `33f504...` / coordinator `a5b2e9...` と一致しない。lock 解消後の再検証では、承認済み DS4 artifact への復帰を先行する。

**2026-08-16 H-01 完了**: `6bde9c10c148b3a85da6c015a5951bf21f3e898e` を両 node の同一 candidate として配置し、candidate binary SHA-256 は両 node とも
`c21bd1934cb531f5f1abd729429c3800492004c7067a2a20f5d2e6a2542a66a0`、LaunchAgent plist SHA-256 は両 node とも
`dfbb42036293a72dae2b5f9d2ebdc2134327055abf349b920ae4e9bd0aa064eb` だった。config checksum は coordinator
`233cca9eab61faeb0c6e5544115ff7cf9675547cd2e2be1070f489085a23aa02`、worker
`a94e778092470cb2fefd7ad91f76e0e6378b3a1f663ec2c40418d008f6444912`。rollback backup は保持され、manifest SHA-256 は
worker `be5f52bf14b5a54a1e1efd378672f93cb5a8966b92096d612b55815711f216f9`、coordinator
`7c61eadb67c783d03c16e79f14b98a183904cc327e0d6bf4ff7ff16e3c265977` である。

両 node の LaunchAgent は candidate path を指し、重複 job はなく、operator 承認済み startup cleanup は worker/coordinator 各 1 件の stale DS4 process に対して実施された。最終状態は両 node とも `SoloStandaloneReady`、`/healthz=ok`、`/readyz=ready`、`cluster doctor` の `healthy=true`（`admission_serving=true`、`safe_state=true`、`target_ready=true`）で、active request は 0 件、standalone DS4 child は node ごとに 1 件、unknown/orphan process はない。最終 worker generation は 278、coordinator generation は 339。最終 evidence は worker `/private/tmp/siderostat-reconnect-evidence-20260815/baseline/20260816-h01-*`（manifest SHA-256 `988ed4fcef3196d355878546adff2cabbb2f633e42d1696b18a68b61f02e70de`）および coordinator `/Users/o/siderostat-reconnect-evidence-20260815/baseline/20260816-h01-*`（manifest SHA-256 `74e56ceee48c3a538c0d80b8f6ddbc4c82580cc961e3ae13396dde0fd19d3e81`）に保存した。これは H-01 完了時点の記録であり、その時点では H-02 は両 node が DistributedReady ではないため未着手だった。

## 4. 証跡ディレクトリと記録方法

証跡は **repository 外**の evidence directory へ置く（plan H-01 Files: repository 外 evidence directory）。
repository 内には secret / token / prompt / request body / 完全 deployment ID を入れない。

### 4.1 証跡ディレクトリ作成

```sh
EVID="$HOME/siderostat-reconnect-evidence"
mkdir -p "$EVID"/{baseline,h02-cable,h03-process,h04-macos,h05-acceptance}
echo "evidence dir: $EVID"
```

### 4.2 記録ルール

- 各操作の前に、その時点の `cluster status --json` と `cluster doctor --json` を保存する。
- ファイル名は `YYYYMMDD-HHMMSS-<node>-<scenario>-<step>.json` 形式にする。
- 保存直後に `shasum -a 256` を同じディレクトリの `SHA256SUMS.txt` へ追記する。
- **redaction**: secret 値・token・prompt・request body・完全 deployment ID は保存内容に含めない。
  ログを保存する場合は、該当行を `grep -v` で除外するか、保存前に目視で確認する。

```sh
cd "$EVID"
siderostat cluster status  --json > baseline/coordinator-status.json
siderostat cluster doctor   --json > baseline/coordinator-doctor.json
shasum -a 256 baseline/*.json >> SHA256SUMS.txt
```

## 5. 観測コマンド集（agent・operator 共通）

### 5.1 状態・健全性

```sh
siderostat cluster status            # 人間向け
siderostat cluster status --json     # 証跡用
siderostat cluster doctor --json     # healthy = target_ready && safe_state && admission_serving
curl --fail --silent http://127.0.0.1:18081/cluster
curl --fail --silent http://127.0.0.1:18081/healthz
curl --fail --silent http://127.0.0.1:18081/readyz
```

### 5.2 ネットワーク（bridge0 / route）

```sh
ifconfig bridge0
netstat -rn | grep bridge0
scutil show State:/Network/Interface/bridge0
```

### 5.3 process / child 確認

```sh
pgrep -fl siderostat          # proxy 本体（1 process 想定）
pgrep -fl ds4-server          # DS4 child（最大 1 process）
ps -o pid,ppid,pgid,etime,command -ax | grep -E 'siderostat|ds4-server' | grep -v grep
```

### 5.4 ログ確認

```sh
tail -n 200 "$HOME/Library/Logs/siderostat/ds4-siderostat.log"
# または unified log（必要時）:
# log show --last 5m --predicate 'process == "siderostat"'
```

### 5.5 public inference smoke request

model / profile は `docs/compatibility/ds4-b030961.md` の利用対象に従い、short prompt で確認する。
**prompt と request body は証跡に含めない**（成功/失敗と status code だけを記録する）。

```sh
# 例: /v1/chat/completions（admin API 経由）。実際の endpoint/model は config と ds4-b030961.md に従う。
curl --silent --output /dev/null --write-out '%{http_code}\n' \
  http://127.0.0.1:18081/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"model":"<model>","messages":[{"role":"user","content":"<short>"}],"max_tokens":1}'
```

## 6. H-01 rollback 準備（operator の作業）

1. change window を決め、進行中 inference request がないことを確認する。
2. candidate の commit SHA / binary SHA-256 / config checksum を agent が記録する。
3. 現行 binary / config / state / log を削除せず backup する。

```sh
# binary backup
cp /usr/local/bin/siderostat "$EVID/rollback/siderostat.$(date +%Y%m%d-%H%M%S)"
shasum -a 256 /usr/local/bin/siderostat >> "$EVID/SHA256SUMS.txt"

# config backup
cp "$HOME/Library/Application Support/siderostat/config.toml" "$EVID/rollback/config.toml"

# state backup（削除せず別名で保存）
cp "$HOME/Library/Application Support/siderostat/cluster-state.json" "$EVID/rollback/cluster-state.json"
```

4. rollback binary と `launchctl` job label を確認する。

```sh
launchctl print "gui/$(id -u)/local.siderostat.runtime" | head -40
launchctl print-disabled "gui/$(id -u)" | grep -i siderostat || true
```

既存の `siderostat` / `ds4-server` process が検出された場合、macOS の右上通知と警告音で
「再起動が必要です（5秒後）」を表示し、既定では5秒後にidentity再確認付きで停止して起動を続行する。
拒否する場合は設定の `[startup_cleanup] auto_restart = false`、または起動時の
`siderostat serve --decline-startup-cleanup` を使用する。

5. baseline を採取する（§5 の status / doctor / process / log 開始位置）。

- 完了条件: candidate / rollback の checksum が記録され、両 node が Solo または Distributed の
  健全な baseline である。
- 停止条件: active workload、拒否指定またはidentity確認できない unknown DS4 child、重複 supervisor、backup 不在、node 時刻の大幅ずれ。

## 7. H-02 cable detach/reconnect（operator の作業）

前提: 両 node が同じ candidate build で DistributedReady、推論停止中。

1. agent が開始 snapshot と child identity を採取する。
2. **operator**: Thunderbolt cable を抜く。network 設定は変更しない。
3. agent が両 node の SoloStandaloneReady / LocalStandalone / Serving / distributed child 不在を確認する。
4. **operator**: 同じ cable を戻す。
5. agent が route/session 再確立、Paired、auto promotion、新規 DistributedReady を順に確認する。
6. 同じ cycle を連続 2 回行う。1 回でも失敗したら回数をリセットせず失敗 evidence を保存する。

operator は cable を抜く・戻す直前と直後に「いま抜きます/戻しました」と agent へ報告する。
agent は各 checkpoint で status/doctor/log を保存する。

- 完了条件: 2 回連続成功し、proxy 各 1、node ごとの DS4 child 最大 1、orphan なし。
- 停止条件: local standalone が ready にならない、unknown process、active user request、温度/容量等の運用警告。

### 7.1 H-02 実施結果（2026-08-16）

candidate commit `8f6c86c`（両 node binary SHA-256: `a848c3d7894c9b5c508be0892ba0e4b9169b2060a327d56d4c493a9aa0de1082`）で、operator が cable detach/reconnect を 2 回連続実施した。各 cycle とも、detach 後に両 node が role loss を検知して `SoloStandaloneReady` / `admission=serving` / distributed child 不在へ復帰し、reconnect 後に pairing、auto promotion、新 generation の `DistributedReady` へ収束した。

- cycle 1: detach 17:15:29 JST、reconnect 17:20:37 JST、最終 worker generation 361 / coordinator generation 429
- cycle 2: detach 17:24:03 JST、reconnect 17:28:28 JST、最終 worker generation 371 / coordinator generation 439
- 最終 child: worker distributed worker PID 50296、coordinator distributed coordinator PID 36953。standalone child は両 node とも停止。
- 最終 control phase は両 node `worker-ready`、lease valid / peer-present / route-scoped。`cluster doctor --json` は両 node `healthy=true`、active request は 0。
- public inference smoke は直列実行で worker/coordinator とも HTTP 200。並列 probe の一方は `max_in_flight=1` により HTTP 503 となったが、直列 retry は HTTP 200 だったため、cycle failure には分類しない。
- worker の stale distributed DS4 cleanup に各 cycle 約 3〜4 分を要したが、local standalone は両回とも ready になり、unknown/orphan process は残らなかった。
- Evidence summary: `/private/tmp/siderostat-reconnect-evidence-20260816/h02/20260816-h02-summary.md`

## 8. H-03 片側 process 再起動（operator の作業）

前提: cable 接続済み、両 node DistributedReady、rollback 可能。

1. coordinator の siderostat process だけを既存 LaunchAgent 手順で再起動する。
2. worker を再起動せず、Solo → session 再確立 → Paired → Distributed の収束を確認する。
3. 新しい PID / session / child generation と orphan 不在を記録する。
4. baseline 再確立後、worker の siderostat process だけで同じ手順を行う。
5. 各方向を 2 回連続で実行する。

LaunchAgent 再起動（既存手順）:

```sh
launchctl kickstart -k "gui/$(id -u)/local.siderostat.runtime"
```

- 完了条件: coordinator-only と worker-only が各 2 回連続成功し、409 loop や古い child 再利用がない。
- 停止条件: `launchctl` job が重複、restart throttle 未経過、runtime state 削除が必要。

### 8.1 H-03 実施結果（2026-08-16、coordinator-only 1 回目）

coordinator の LaunchAgent だけを再起動した。coordinator は `SoloStandaloneReady` へ復帰したが、worker の distributed child 停止が `SIGKILL is not allowed for this child` となり、約 180 秒後の recovery retry で worker は `SoloStandaloneReady` へ戻った。その後、worker は `paired` だが lease invalid / peer absent、coordinator は `unpaired` のまま `/v1/node` HTTP 409 (`peer has not established a control lease`) が継続し、自動 re-pair / promotion / `DistributedReady` に収束しなかった。

このため H-03 の停止条件に従い、worker-only restart と残りの cycle は実施していない。両 node は `SoloStandaloneReady`、admission serving、active request 0、`cluster doctor healthy=true`、standalone child 各1件、LaunchAgent 各1件の安全な状態に戻っている。手動 pair/reconcile、state 削除、force kill は実施していない。

Evidence: `/private/tmp/siderostat-reconnect-evidence-20260816/h03/cycle1-coordinator-only-failure.md`

### 8.2 H-03 修正適用・再開準備（2026-08-16）

失敗原因に対して、失効した認証済み peer の `/v1/node` を再 pair 用 descriptor 応答へ変更し、PeerLost recovery 後の control phase を lease 有効時だけ `Paired` とするよう修正した。auto pair は coordinator のみが実行し、DS4 child の SIGTERM 後に残留した場合は `allow_sigkill=true` の identity 再確認済み owned child を SIGKILL できる設定へ両 node を更新した。

両 node を candidate `1952cd7bb2db07ffcb4e5487fed3a52949f3d2d7f85b4c35a57f7391fc4f3cb0`、`allow_sigkill=true`、更新済み LaunchAgent で再起動した。両 node は一度 `SoloStandaloneReady` へ起動後、coordinator 起点の自動 re-pair、lease valid / peer-present / route-scoped、auto promotion、`DistributedReady`、`cluster doctor --json` の `healthy=true` へ収束した。control generation は両 node `445`、distributed child は worker PID `69260` / coordinator PID `39175` の各1件だった。

この確認は修正適用と H-03 再開条件の確認であり、coordinator-only / worker-only 各2 cycleの完了記録ではない。詳細 evidence: `/private/tmp/siderostat-reconnect-evidence-20260816/h03/20260816-h03-fix-application.md`

### 8.3 H-03 完了（2026-08-16）

修正適用後、coordinator-only 2 cycle と worker-only 2 cycle を実施した。各 cycle は片側 process 再起動後に peer loss recovery、SoloStandaloneReady、自動 re-pair、auto promotion、DistributedReady へ収束した。各最終 checkpoint で両 node の `cluster doctor --json` は `healthy=true`、admission serving、active request 0、lease valid / peer-present / route-scoped、public `/v1/models` は HTTP 200 だった。

distributed child は各 cycle で新しい PID/generation へ更新され、最終は worker PID `75852`、coordinator PID `40229`。LaunchAgent の runtime job は各 node 1件、orphan child はなく、新しい検証時間帯に 409 loop、`SIGKILL is not allowed for this child`、古い child 再利用はなかった。再起動直後の coordinator route-loss demotion task の終了競合ログは記録されたが、shared recovery と最終収束は成功した。

Evidence: `/private/tmp/siderostat-reconnect-evidence-20260816/h03/20260816-h03-summary.md`

## 9. H-04 macOS 再起動（operator の作業）

前提: process 再起動の両方向が成功し、operator が長い中断を承認。

1. DistributedReady から coordinator の macOS だけを再起動し、worker は稼働継続する。
2. login / LaunchAgent 起動後、両 node の自動復帰を確認する。
3. baseline 再確立後、worker の macOS だけを再起動して同じ確認を行う。
4. baseline 再確立後、両 macOS を再起動して同じ確認を行う。
5. boot 前後の永続 cluster/control generation、child identity、409 の有無を比較する。

macOS 再起動は operator が行い、agent は再起動前後の観測を担当する。再起動は
`システム設定 > 再起動` または `sudo shutdown -r now`（operator 判断）。

- 完了条件: 片側再起動の両方向と両側再起動が、手動 pair/reconcile や state 削除なしで
  DistributedReady へ戻る。
- 停止条件: OS update、別 service、disk encryption unlock 等が検証条件を変える。

### 9.1 H-04 完了（2026-08-17）

coordinator-only（Mac Studio）、worker-only（MacBook Pro）、両 node 同時再起動の3ケースを完了した。各ケースとも login/LaunchAgent 復帰後、手動 pair/reconcile・state 削除・force kill なしで自動 pairing、promotion、`DistributedReady` へ収束した。最終 checkpoint は両 node `cluster doctor --json` `healthy=true`、admission serving、in-flight 0、lease valid / peer-present / route-scoped、public `/v1/models` HTTP 200、LaunchAgent 各1件、DS4 child 各1件だった。

両 node 同時再起動後の boot time は worker `2026-08-17 09:03:42`、coordinator `2026-08-17 09:03:40`。最終 worker child は PID `2578` / generation `449`、coordinator child は PID `1068` / generation `607`、control generation は両 node `601`。boot 後の log window に 409 loop、`SIGKILL is not allowed for this child`、orphan child はなかった。promotion 中の一時的な admission blocked / HTTP 503 は最終 checkpoint 前に serving / HTTP 200 へ復帰した。

Evidence: `/private/tmp/siderostat-reconnect-evidence-20260817/h04/20260817-h04-summary.md`

## 10. H-05 evidence 集約と acceptance 判定

1. scenario ごとに build、操作、所要時間、generation、child identity 変化、結果を表にする。
2. failure / retry / manual intervention の有無をすべて記載する。
3. raw artifact の path と SHA-256 を記載し、secret / 個人情報を repository に入れない。
4. operator が candidate 継続利用または旧 binary への rollback を選ぶ。
5. rollback を選んだ場合も state/model/cache を削除せず、旧 binary で Solo readiness を確認する。

- agent が redaction 済み acceptance 文書 `docs/compatibility/reconnect-acceptance-YYYY-MM-DD.md` を作成する。
- operator が実機 PASS/FAIL と candidate の扱いを承認する。
- 停止条件: evidence 欠落を推測で PASS にする必要がある。

### 10.1 H-05 完了（2026-08-17）

H-01〜H-04 の実機結果を proposal 第1節の5 acceptance criteriaへ対応付け、すべて PASS と判定した。operator は rollback を実施せず、検証済み candidate を `develop` merge 後の Release Candidate packaging へ進めることを承認した。rollback binary/config/state/plist は緊急復旧用に保持し、削除していない。

Acceptance record: `docs/compatibility/reconnect-acceptance-2026-08-17.md`

## 11. operator からの報告テンプレート

各操作後、次の形式で agent へ報告する。

```text
操作: <cable detach | cable attach | process restart | macOS restart | binary 配置承認 | rollback>
node: <coordinator | worker | both>
時刻: <YYYY-MM-DD HH:MM:SS JST>
内容: <何を行ったか>
結果: <成功 | 失敗 | 中止> + <status code / 状態 / 観測>
証跡: <保存したファイル名 / path>
特記事項: <なければ「なし」>
```

## 12. 停止・エスカレーション条件

次の場合は作業を推測で続けず、failure evidence を保存して agent へ報告する（plan 第15節）。

- protocol / 永続 schema / role authority / timeout semantics が設計文書で一意でない。
- state と child lifecycle の整合を保つために unknown process への signal が必要。
- 実機で model / binary / deployment compatibility が両 node で一致しない。
- user workload が開始した、または物理 cable / OS / service 操作の安全な時間が終了した。
- secret / token / prompt / request body / 完全 deployment ID が artifact に混入した。
- runtime state / model / KV cache / legacy data の削除が必要。

## 13. 実機検証を始める前の確認リスト

- [ ] 共通 local gate と reconnect acceptance suite が GREEN（agent 確認）
- [ ] 両 node の candidate build が一致（SHA-256 記録）
- [ ] change window が確保され、進行中 inference request がない
- [ ] rollback binary / config / state が backup 済み
- [ ] `launchctl` job label が確認済み（重複なし）
- [ ] 証跡ディレクトリを作成し、SHA256SUMS.txt を開始した
- [ ] 両 node の時計が大きくずれていない
- [ ] baseline status / doctor / process / log 開始位置を採取した
