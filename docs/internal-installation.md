# Siderostat 導入ガイド（v0.3.0）

この文書は、配布 DMG からの通常インストールと、開発者・operator が実 DS4/model を用いて
2-node の `DistributedReady` へ到達する検証手順を定める。エンドユーザーは「通常の DMG 導入」
だけを実施し、後半の checkout/build/secret/config 手順は開発・検証時だけ使用する。

## 0. 通常の DMG 導入（エンドユーザー）

配布物は署名・公証済みの `Siderostat-<version>.dmg` である。DMG の直下には次の3項目だけがある。

- `Siderostat-<version>.pkg`
- `Siderostat Uninstaller.app`
- `README.html`

両 node で次を行う。

1. DMG を Finder で開き、`Siderostat-<version>.pkg` をダブルクリックする。
2. macOS Installer の確認・管理者認証を完了する。package は置換前に product-owned の runtime と
   Siderostat Monitor だけを停止し、ユーザーデータや model は変更しない。
3. インストール成功後、ログイン中の GUI session があれば `/Applications/Siderostat.app` が起動する。
4. 初回起動の説明・通知に従い、runtime background item と Siderostat の Login Item の登録状態を確認する。
5. macOS が承認を要求した場合は System Settings > General > Login Items で承認する。package の
   管理者認証と Login Items / Background Items のユーザー承認は別の操作である。
6. メニューの `Status`、`Mode`、`State` が正常になった後、両 node の `DistributedReady` を確認する。

### 旧版からの更新・移行

旧版の `/usr/local/bin/siderostat`、`siderostat-monitor`、`local.siderostat.*` LaunchAgent が残っている場合は、
first launch が旧インストールの存在を通知する。移行は旧 job の PID・実行ファイル identity を確認してから行い、
旧 runtime を drain、旧 job を停止し、旧 plist を
`~/Library/Application Support/siderostat/migration-backup/` へ一意の名前でバックアップしてから新しい
background item を登録する。identity を確認できない process は自動停止の対象にしない。

新しい service の登録または readiness 確認に失敗した場合は、旧 plist と旧 job を復元する rollback 経路へ
収束する。移行成功後も旧 binary は自動削除せず、削除可能であることだけを表示する。設定、secret、manifest、
cluster state、model、KV cache、旧 affinity database は移行・更新・rollback で削除しない。旧設定の unknown
field や曖昧な複数 backend は黙って変換せず、表示されたエラーを解消してから再実行する。

アンインストールは同じ DMG の `Siderostat Uninstaller.app` を Finder から起動する。確認画面で承認すると、
product-owned の background/login item、runtime、managed `ds4-server` child、アプリ bundle と package
receipt を整理する。`Application Support`、secret、manifest、cluster state、model、KV cache は保持する。
エラーが表示された場合は表示された状態を解消して同じ Uninstaller を再実行し、`sudo rm -rf` や `killall`
は使用しない。

通常 package は bundle version check を行うため、旧版への downgrade には使用しない。rollback は配布側が
明示した `Siderostat-<version>-rollback.pkg` だけを使用する。

`cargo xtask install`、`cargo xtask app-dev`、`cargo xtask pkg-dev` は開発・検証用であり、エンドユーザーの
通常インストール手順ではない。詳細は [`xtask/README.md`](../xtask/README.md) を参照する。

## 対象と前提

- 対象platform: macOS 26系、Apple Silicon。
- 対象topology: Thunderbolt Bridgeで直結した2 node。M4 Max 128GB coordinator、M5 Max 128GB worker、Thunderbolt 5直結を基準とする（spec第32.5節）。
- Role addressは`bridge0`のIPv4から決定する。`10.99.0.1`がcoordinator、`10.99.0.2`がworker、その他/未設定/競合はunknown（spec第13.2節）。Roleはconfigで指定しない。
- Proxyは`rewrite/mode-aware`branchでbuildする。

v0.1.0の実DS4 source baselineはfull commit `b0309611041655f4e45671cfd9c9886aff161406`である。利用対象profile、機種別native binary digest集合、distributed acceptanceの結果は`docs/compatibility/ds4-b030961.md`で確認する。Model配布条件や未確認URLは推測しない。

## 1. 開発・検証者向け: DS4 checkout/build、digest記録

以下は実 DS4/model を使った acceptance または release candidate 検証用である。通常の DMG 導入だけを
行う利用者はこの節を飛ばす。

対象Macで次の前提を確認する。

- macOS 26系、Apple Silicon。
- Rust stable（edition 2024）が利用可能。
- Thunderbolt Bridge `bridge0`がSystem Configurationのnetwork serviceとして存在し、enabledである。

DS4 binaryはoperatorの既知のsourceから取得する。Repository URLや配布条件は推測しない。対象commitをfull SHAへ固定する。

```sh
DS4_CHECKOUT="/absolute/path/to/ds4-checkout"
git -C "$DS4_CHECKOUT" rev-parse HEAD
git -C "$DS4_CHECKOUT" checkout b0309611041655f4e45671cfd9c9886aff161406
git -C "$DS4_CHECKOUT" rev-parse HEAD
```

`rev-parse HEAD`がverified baseline `b0309611041655f4e45671cfd9c9886aff161406`と一致しない場合、baselineを更新せず作業を停止する。一致する場合のみ続行する。

DS4 binaryをbuildし、digestを記録する。Build手順はoperatorの既知のsourceに従う。Binary pathとSHA-256を`docs/compatibility/ds4-b030961.md`へ追記する。

```sh
DS4_BINARY="/absolute/path/to/ds4-server"
shasum -a 256 "$DS4_BINARY"
```

Sanitized CLI確認だけを保存する。Model path、user path、prompt、body、secretは記録しない。

```text
<absolute-ds4-binary> --help
<absolute-ds4-binary> --help distributed
sha256(<absolute-ds4-binary>)
git -C <ds4-checkout> rev-parse HEAD
```

Supervisorが使用予定のoptionがCLI helpに存在することを確認する（`docs/compatibility/ds4-b030961.md`のCLI compatibility一覧）。`--version`はunknown optionとして拒否されるため、binary単体ではなくsource checkoutでcommitを確定する。

## 2. Model選択/取得/checksum/配置

Standalone profileとdistributed profileは独立した設定である（spec第14.1節）。

- Standalone quantization: `q2`、`q2-q4`、`mxfp4`。
- Standalone residency: `resident`または`ssd-streaming`。
- Production Standalone: resident + DSpark support GGUF（現行DS4ではSSD streamingおよびDistributedとの併用不可）。
- Distributed profile: `distributed-layer-parallel`（topology=`layer-parallel`、quantization=`mxfp4`、speculative support=`none`）。

Modelはoperatorの既知のsourceから取得する。URLや配布条件は推測しない。取得後、両nodeでchecksumとsizeを記録する。

```sh
MODEL_PATH="/absolute/path/to/model.gguf"
shasum -a 256 "$MODEL_PATH"
ls -l "$MODEL_PATH"
```

配置したstandalone / MXFP4 / DSpark support GGUFをsiderostatのinstall用cacheへ記録する場合は、repositoryで次を一度実行する。モデルの内容を読み、digestとファイルメタデータだけを`~/Library/Application Support/siderostat/manifests/digest-cache.json`へ保存する。モデルを置換・変更した場合だけ再実行する。

```sh
cd /absolute/path/to/siderostat
cargo xtask fingerprint-models --ds4-server "/absolute/path/to/ds4-server"
```

`cargo xtask install`はGGUFのSHA-256計算について確認を表示し、既定の`N`ではこのcacheを使う。metadataが完全一致すれば即時再利用し、inodeやmtimeだけが変わってsizeが同じ場合は最大4 MiBの分散サンプル署名を確認する。一致時はcache metadataを自動更新し、GGUF全体を再読込しない。size変更またはサンプル不一致時だけ、`cargo xtask fingerprint-models`の実行を要求する。`Y`または非対話の`cargo xtask install --hash-models`はfull SHA-256を再計算する。

旧形式cacheにはサンプル署名がない。modelが移動・複製・`touch`されただけで内容不変だとoperatorが確認済みの場合は、`cargo xtask install --accept-model-metadata-change`を一度指定してcached full digestを維持したままmetadataを更新できる。この指定でもsize変更は受理しない。内容に確信がない場合はfull SHA-256を再計算する。サンプル署名はmetadata drift時の高速な偶発変更検出であり、full-file SHA-256と同等の保証ではない。

Distributed (layer-parallel) に用いる両nodeのmodel content SHA-256を一致させる（spec第14.2節）。現行配布では約156GBのMXFP4 GGUFを両nodeへ配置する（MXFP4はquantizationであり、topologyではない）。不一致ならlayer-parallel promotionを拒否し、両nodeをSolo Standaloneへ収束させて自動pairingを停止する（spec第15.3節）。DSpark support GGUFはspeculative supportの独立項目としてchecksum/sizeを記録し、そのnodeのStandalone manifestへ設定する。DS4 binaryはnode別digestを記録し、byte-for-byte一致ではなく、actual acceptance済みdigestだけを両manifestの同一 `compatible_ds4_binary_sha256` 集合へ昇順で記載する。未知rebuildを自動追加しない。各nodeのinstallでは `--peer-ds4-binary-digest` に相手nodeのdigestだけを指定し、自nodeのdigestはinstallが計算して集合へ追加する。

Modelはcanonical absolute pathで指定し、書換可能なsymlinkを使わない（spec第14.2節、第22.3節）。配置先は`siderostat.example.toml`のplaceholder pathへ合わせる。

Manifestは`docs/spec.md`第15.1節のschemaに従い、standaloneとdistributedのdigest情報をJSONで作る。DSpark有効なStandalone manifestには`dspark_enabled`、support digest/size、confidence、strictを記録する。起動時に実support fileとtyped configへ一致しなければchildはspawnされない。Standalone manifestはpeer間compatibilityの比較対象にしない（spec第15.3節）。

## 3. Resident/SSD streaming standalone smoke

実DS4とmodelを使うため、この手順はoperator gateである。`docs/compatibility/ds4-b030961.md`のModel/profile matrixを基準とし、次の対象を確認する。

- Q2-Q4 resident standaloneでrequest成功。Q2は対応するfull standalone modelを利用する構成だけで追加確認する。
- Q2-Q4 resident + DSparkでsanitized `dspark-activated` log、HTTP readiness、short requestを確認する。
- Q2-Q4 SSD streaming standaloneでrequest成功。
- MXFP4 SSD streaming standaloneでrequest成功。対象DS4 build/Metal backendで未確認の場合は、そのprofileだけをproduction enable不可とする。
- `ssd-streaming`はDS4の`--ssd-streaming`を意味し、model variantと混同しない（spec第14.1節）。`residency="resident"`ではSSD streaming optionを生成しない。
- HTTP readiness、short prompt、streaming、memory/startup timeを確認する。

実測結果を`docs/compatibility/ds4-b030961.md`のModel/profile matrixへ追記し、productionで利用するprofileのstatusをPASSへ更新してから次へ進む。利用対象外のQ2 resident、Q2 SSD streaming、MXFP4 residentはrelease gateにしない。

## 4. Thunderbolt固定IPv4、bridge/route確認

Proxyはnetwork設定を作成、有効化、address変更しない（spec第13.1節）。OperatorがSystem Settingsで固定IPv4を設定する。`Thunderbolt Bridge`という表示名だけでは識別しない。

確認項目（spec第13.1節）:

1. System Configuration preferences内に、BSD interface `bridge0`に対応するnetwork serviceが存在する。
2. `SCNetworkServiceGetEnabled`がtrueである。
3. IPv4 protocolが有効で、期待するaddress設定と矛盾しない。
4. Runtime stateに`bridge0`が存在し、UPである。
5. `bridge0`に期待するIPv4 address/prefixが付与されている。
6. Peer candidateへのrouteが`bridge0` scopedである。

- Coordinator: `bridge0=10.99.0.1`。
- Worker: `bridge0=10.99.0.2`。

確認は診断目的で行い、通常経路のtext parseにしない。

```sh
ifconfig bridge0
netstat -rn | grep bridge0
scutil show State:/Network/Interface/bridge0
```

`ReadyNoPeer`以前の状態をpeer presentとして扱わない。`AuthenticatedPeer`だけがpairingを開始できる（spec第13.1節）。

Appleの公式手順を参照する（spec第39節）: [Apple: ThunderboltでIPを使ってMacコンピュータを接続する](https://support.apple.com/ja-jp/guide/mac-help/mchld53dd2f5/mac)。

## 5. Proxy build、secret、config、foreground test

### Proxy build

Rust stable（edition 2024）でbuildする。Common local gateを実行する。

```sh
REPOSITORY="/absolute/path/to/siderostat"
cd "$REPOSITORY"
cargo build --release
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
git diff --check
```

runtime binaryは`target/release/siderostat`、monitor binaryは`target/release/siderostat-monitor`である。
LaunchAgentの`ProgramArguments`が参照する`/usr/local/bin/siderostat`と`/usr/local/bin/siderostat-monitor`へinstallする。
`/usr/local/bin`への書き込みに管理者権限が必要な環境では、ownerとmodeを固定する。

```sh
sudo install -d -m 0755 /usr/local/bin
sudo install -m 0755 target/release/siderostat /usr/local/bin/siderostat
sudo install -m 0755 target/release/siderostat-monitor /usr/local/bin/siderostat-monitor
/usr/local/bin/siderostat --help
/usr/local/bin/siderostat-monitor --help
```

### Secret file

Secret/token fileは各32 bytes以上の生バイト列、mode `0600`、相互に異なるpathで配置する（spec第22.3節、第32.5節）。これらはSSH秘密鍵やPEMではなく、control/peer認証用のHMAC secretとadmin API用tokenである。標準名は拡張子なしの`cluster-control`、`peer-proxy`、`admin`とする。Control secretとpeer proxy tokenは、それぞれclusterの両nodeで同じ値が必要である。Admin tokenはnode-localとする。Control secret、peer proxy token、admin token間でfileまたは値を流用しない。

まずcoordinator上で共有する2値とcoordinatorのadmin tokenを生成する。`umask 077`により生成時からownerだけが読み書きできる。

```sh
SECRET_DIR="$HOME/Library/Application Support/siderostat/secrets"
umask 077
mkdir -p "$SECRET_DIR"
openssl rand -out "$SECRET_DIR/cluster-control" 32
openssl rand -out "$SECRET_DIR/peer-proxy" 32
openssl rand -out "$SECRET_DIR/admin" 32
chmod 600 "$SECRET_DIR/cluster-control" "$SECRET_DIR/peer-proxy" "$SECRET_DIR/admin"
```

`cluster-control`と`peer-proxy`を、operatorが承認した暗号化済み媒体または同等の安全な経路でworkerへ移す。Shell history、clipboard manager、repository、plist、command lineにsecret値そのものを記録しない。Workerでは共有2 fileを同じfilenameで`SECRET_DIR`へ配置し、admin tokenだけをworker上で新規生成する。

```sh
SECRET_DIR="$HOME/Library/Application Support/siderostat/secrets"
SHARED_SECRET_SOURCE="/Volumes/OPERATOR-APPROVED-ENCRYPTED-MEDIA"
umask 077
mkdir -p "$SECRET_DIR"
install -m 0600 "$SHARED_SECRET_SOURCE/cluster-control" "$SECRET_DIR/cluster-control"
install -m 0600 "$SHARED_SECRET_SOURCE/peer-proxy" "$SECRET_DIR/peer-proxy"
openssl rand -out "$SECRET_DIR/admin" 32
chmod 600 "$SECRET_DIR/cluster-control" "$SECRET_DIR/peer-proxy" "$SECRET_DIR/admin"
```

既存環境の`cluster-control.key`、`peer-proxy.key`、`admin.key`はそのまま保持できる。
`cargo xtask install`は対応するlegacy fileを検出すると、値を拡張子なしのcanonical fileへ複製して
新しいconfigから参照する。共有secretを手動で移行する場合も、値を再生成せずbyte-for-byteで複製する。

両nodeの共有2 fileがbyte-for-byteで一致し、各node内の3 fileがそれぞれ異なることを安全な経路上で確認する。Digestやsecret値はdocumentation evidenceに保存しない。

### Config

`siderostat.example.toml`をnode別の作業fileへcopyし、全ての`PLACEHOLDER`を実在するabsolute pathへ置換する。`$HOME`と`~/`だけを展開する（spec第22.1節）。Worker nodeは`cluster.node_id`とnode固有pathだけを変更する。Roleはinterface addressから決定し、設定で直接指定しない（spec第22.2節）。

```sh
cp siderostat.example.toml "$HOME/Library/Application Support/siderostat/config.toml"
```

置換対象：

- `ds4.binary`の`PLACEHOLDER-ds4-server`。
- `ds4.dspark.support_model`の`PLACEHOLDER-dspark-support-0731.gguf`。
- `ds4.standalone.model`と`ds4.distributed.model`の`PLACEHOLDER-*.gguf`。
- Secret fileの`PLACEHOLDER-cluster-control`、`PLACEHOLDER-peer-proxy`、`PLACEHOLDER-admin`。両nodeで共有するcontrol/peerの値とnode-localのadmin値を参照する。
- Manifest pathの`PLACEHOLDER-standalone.gguf`相当のmanifest JSON。

Validation要件（spec第22.3節）を満たすことを確認する。

- `schema_version == 2`。
- 各portが衝突しない。
- DS4 binaryがregular executable file。
- Model、DSpark support GGUF、manifestがregular file、canonical absolute path、書換可能symlinkでない。
- Secret/token fileが各32 bytes以上、mode `0600`、相互に異なる。
- Timeoutが0/無制限でない。
- `extra_args`が生成引数を上書きしない。
- DSpark有効時はStandaloneが`resident`、confidenceが0以上1以下で、support fingerprint/configがStandalone manifestと一致する。

### Foreground test

Foregroundで起動し、起動形式を確認する。Subcommandなしは`serve`と同じ（spec第23.2節）。

```sh
siderostat --config "$HOME/Library/Application Support/siderostat/config.toml"
# または
siderostat serve --config "$HOME/Library/Application Support/siderostat/config.toml"
```

Admin APIでhealth/readiness/cluster stateを確認する。

```sh
curl --fail --silent http://127.0.0.1:18081/healthz
curl --fail --silent http://127.0.0.1:18081/readyz
curl --fail --silent http://127.0.0.1:18081/cluster
siderostat cluster status
siderostat cluster doctor
```

`doctor`がThunderbolt IP readiness、discovery、state、target readinessを報告する。`ReadyNoPeer`以前はpeer presentにしない。Secretをlogしない（spec第32.4節）。

Standalone起動完了後の期待結果は次のとおりである。起動中の一時的な503は完了まで待って再確認する。

- `/healthz`: HTTP 200、`{"status":"ok"}`。
- `/readyz`: HTTP 200、`status="ready"`、`target_ready=true`、`admission="serving"`。
- `cluster status`: `role`がcoordinator/workerの期待role、`mode=solo-standalone`、`state=solo-standalone-ready`、`ready=true`。
- `cluster doctor`: `doctor=ok state=solo-standalone-ready target_ready=true`。

## 6. Pairing、promotion、LaunchAgent、recovery、upgrade、rollback、uninstall

### macOS app / package install

配布 DMG 内の `.pkg` を Finder の Installer で開くか、次のコマンドで `/Applications` へ
インストールする。

```sh
sudo /usr/sbin/installer -pkg "/path/to/Siderostat-0.3.0.pkg" -target /
```

既存の `/Applications/Siderostat.app` が起動中なら、package の `preinstall` が最初に現在の
GUI ユーザーの `gui/<uid>/dev.siderostat-ds4-proxy.runtime` だけを LaunchAgent から unload
し、その後 Monitor を終了してから bundle を置き換える。runtime job と完全一致する bundle 内
runtime executable が残る場合、および Monitor が通常終了しない場合だけ、対象を限定した強制終了を
行う。設定、secret、model、cache、Service Management の恒久的な登録状態は変更・削除しない。
インストール成功後は `postinstall` が、更新前に runtime が `enabled` だった場合だけ新しい
bundle 内の LaunchAgent を同じ GUI domain へ再登録し、その後現在ログイン中の GUI ユーザーの
session で Siderostat を自動起動する。disabled、未登録、承認待ちの場合は bootstrap せず、
アプリが既存の登録・承認状態を読み取って `SMAppService` の導線を表示する。承認が必要な場合は
System Settings > General > Login Items を開く。

ログイン中の GUI ユーザーがいない状態での install は成功するが、自動起動はスキップされる。
その場合は、ログイン後に `/Applications/Siderostat.app` を一度起動する。package の管理者承認と
Login Items / Background Items のユーザー承認は別の macOS 操作である。

### Pairing

両nodeでSolo Standalone ready後、Bonjour discoveryが`bridge0`に限定される。Bonjour結果だけではpairingせず、`bridge0` route、HMAC control handshake、leaseを必須とする（spec第13.3節、第38節）。`AuthenticatedPeer`でpairing候補になり、peer stability（既定5秒）後にpairする。

```sh
siderostat cluster status
siderostat cluster doctor
siderostat cluster pair
```

WorkerはtargetがCoordinatorへ、Coordinatorはtarget=LocalStandaloneのままPairedStandaloneReadyへ入る（spec第18.2節）。両nodeのstandalone profileが異なってもpairingできる（spec第14.1節、第32.5節）。

Pairing完了後は両nodeで`mode=paired-standalone`、`state=paired-standalone-ready`、`ready=true`を確認する。Workerの`target=coordinator`、coordinatorの`target=local-standalone`とならない場合はpromotionへ進まない。

### Promotion

layer-parallel promotionにはdeployment matchと実HELLOとcomplete routeが必須（spec第33.3節）。まずfingerprintを実行する。

```sh
siderostat cluster fingerprint --profile standalone
siderostat cluster fingerprint --profile distributed
```

Fingerprint jobは202 + job IDを返し、同一profileの同時jobを拒否する（spec第23.1節）。数百GBを読む処理をHTTP handler内で同期実行しない（spec第15.2節）。

Binary/model/checkpoint/context/layer split/wire schema/argv profileが一致する場合だけ、実HELLOを受けてpromotionする（spec第15.3節）。`cluster promote`はHELLO/compatibility条件を迂回しない（spec第23.2節）。実HELLOなしでpromotionしない。

```sh
siderostat cluster promote
```

Cluster-wide drain後にDS4を停止し、coordinator の layer-parallel child を起動（`--debug`）、worker registered、complete route readyでDistributedReadyへ入る（spec第18.3節）。HTTP listeningだけでDistributedReadyにしない。

Promotion完了後は両nodeで`mode=distributed-layer-parallel`、`state=distributed-ready`、`ready=true`を確認する。Fingerprint commandは受付時にjob ID付きJSONを返す。Compatibility不一致、HELLO timeout、route incompleteのいずれかがある場合は`distributed-ready`を期待せず、[`docs/internal-troubleshooting.md`](internal-troubleshooting.md)に従う。

### 開発・legacy LaunchAgent（エンドユーザーは使用しない）

配布済み `Siderostat.app` は bundle 内 plist と `SMAppService` で runtime background item と
Siderostat の Login Item を管理する。次の手動 plist 登録は、bundle 外の開発 binary を検証する場合だけ
使用する。エンドユーザーがこれを併用すると duplicate Monitor、duplicate runtime、port conflict の原因になる。

`contrib/launchd/README.md`に従い、1つのuser service jobだけを登録する。DS4 childはproxyが所有・検証・停止するため、`ds4-server`用のplistや同じlisten portを使う別jobを作成しない（spec第35節）。Exampleの`USERNAME`はそのままでは動作しないため、登録前に次の手順で全pathを実在値へ置換する。

- `RunAtLoad=true`、`KeepAlive=true`。
- Absolute ProgramArguments。
- Finite restart throttle（既定10秒）。
- Secret/tokenをplist、`EnvironmentVariables`、command lineへ書かない。

```sh
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/siderostat"
PLIST="$HOME/Library/LaunchAgents/local.siderostat.runtime.plist"
CONFIG="$HOME/Library/Application Support/siderostat/config.toml"
cp contrib/launchd/local.siderostat.runtime.plist "$PLIST"
/usr/libexec/PlistBuddy -c "Set :ProgramArguments:3 $CONFIG" "$PLIST"
/usr/libexec/PlistBuddy -c "Set :StandardOutPath $HOME/Library/Logs/siderostat/ds4-siderostat.log" "$PLIST"
/usr/libexec/PlistBuddy -c "Set :StandardErrorPath $HOME/Library/Logs/siderostat/ds4-siderostat.log" "$PLIST"
plutil -lint "$PLIST"
if grep -Eq 'USERNAME|PLACEHOLDER' "$PLIST"; then echo "unresolved LaunchAgent placeholder" >&2; exit 1; fi
launchctl bootstrap "gui/$(id -u)" "$PLIST"
launchctl kickstart -k "gui/$(id -u)/local.siderostat.runtime"
```

メニューバーモニターも同じ `gui/<uid>` ドメインへ登録する。

```sh
MONITOR_PLIST="$HOME/Library/LaunchAgents/local.siderostat.monitor.plist"
cp contrib/launchd/local.siderostat.monitor.plist "$MONITOR_PLIST"
sed -i '' "s/USERNAME/$(id -un)/g" "$MONITOR_PLIST"
mkdir -p "$HOME/Library/Logs/local.siderostat.monitor"
plutil -lint "$MONITOR_PLIST"
launchctl bootstrap "gui/$(id -u)" "$MONITOR_PLIST"
launchctl kickstart -k "gui/$(id -u)/local.siderostat.monitor"
```

開発 binary のモニター操作は次の LaunchAgent 操作に対応する。

- `Proxy 再起動`: `local.siderostat.runtime` を `kickstart -k`
- `Monitor 再起動`: `local.siderostat.monitor` を `kickstart -k`
- `終了`: runtime と monitor の両方を `bootout`

Verification（`contrib/launchd/README.md`）:

1. Login後にproxyが1 processだけ起動する。
2. `launchctl kickstart -k`後もproxyが1 process、DS4 childが最大1 processである。
3. Login後にmonitorが1 processだけ起動し、メニューバーへ表示される。
4. Proxyを終了すると10秒以上のthrottleを伴って再起動し、orphan DS4 childを残さない。
5. `launchctl print-disabled "gui/$(id -u)"`と`$HOME/Library/LaunchAgents`に、同じbinary/portやDS4 childを所有する別jobがない。

Login起動、proxy restart、no duplicate childはoperator gateである（GUI user session変更が必要）。

### Recovery

- Thunderbolt cable detachで両nodeがlocal standaloneへ復帰する。Address/route喪失またはlease失効でSolo Standaloneへ収束する（spec第18.5節）。
- Route loss grace（既定15秒）経過後はPaired Standaloneへ復帰する（spec第18.4節）。
- Cable再接続後、debounceしたpeer discoveryが再評価され、Paired Standaloneへ、次いで
  `Distributed (layer-parallel)` profileへ再昇格する（spec第32.5節）。MXFP4はそのprofileで使用する
  quantizationであり、topology名ではない。
- 単一eventを「peer接続済み」「peer切断済み」と解釈しない（spec第13.5節）。

### Upgrade

DS4 update時は`docs/spec.md`第36節のcompatibility trackingに従う。

- Verified DS4 commit、binary digest、wire/log fixture digest、recognized event、tested model/topology、dateを`docs/compatibility/ds4-b030961.md`へ記録する。
- `ds4_distributed.c/.h`、model ID/name/quant/layer API、server signal handling、server/distributed log、GGUF/checkpoint、CLI option、distributed QAを確認する。
- Unknown changeではpromotionをfail closedにする。

### Rollback

- Legacy config v1の`backends`、`routing`、`affinity`、`heartbeat`、`active_probe`、`cooldown`、SQLite pathは廃止されている（spec第22.4節）。Unknown/legacy fieldを黙って無視しない。
- 旧affinity databaseを自動削除しない。
- 新configは旧configと分離し、`schema_version == 2`の作業fileを保持する。
- Binary rollbackは直前のbinaryを残し、standalone readinessを確認してから行う。Upgrade後にrollbackし、再度upgradeする（plan P7-02）。
- macOS app bundle の downgrade は通常の `.pkg` では行わない。配布側で `cargo xtask sign --rollback`
  により生成した `Siderostat-<version>-rollback.pkg` だけを明示的な rollback artifact として使用する。
  通常 package は bundle version check を維持し、runtime、LaunchAgent、設定、secret、model、cache は
  installer の対象外である。

### Uninstall

エンドユーザー向けの標準手順は、リリース DMG の `Siderostat Uninstaller.app` を Finder から
起動することである。確認ダイアログで承認すると、Uninstaller は次を順に実行する。

1. `SMAppService` の `siderostat-runtime` LaunchAgent と Siderostat のログイン項目を解除する。
2. identity を確認した `Siderostat` Monitor、runtime、管理対象 ds4-server child を停止する。
3. `/Applications/Siderostat.app` だけを Finder の Trash へ移動する。
4. Siderostat の package receipt だけを整理する。

Application Support、設定、secret、manifest、cluster state、model、KV cacheは削除しない。
処理に失敗した場合はアプリ bundleを残し、表示された状態を解消して同じ Uninstaller を再実行する。
Uninstaller は二重実行にも対応する。Terminal の `launchctl bootout` や手動の削除は、実機診断時を
除きエンドユーザー向け手順ではない。

```sh
# 開発・診断専用。通常の Uninstaller の代わりに実行しない。
launchctl bootout "gui/$(id -u)/local.siderostat.runtime"
mv "$HOME/Library/LaunchAgents/local.siderostat.runtime.plist" "$HOME/Library/LaunchAgents/local.siderostat.runtime.plist.disabled"
```

### 通知と将来機能の制約

通知は recovery epoch 単位で重複を抑制する。`Standalone`、`Paired`、最終 `DistributedReady` は
同一 epoch で一度だけ通知され、recovery failure、manual intervention、deployment mismatch は抑制しない。
通知送信の失敗は cluster state や admission を変更しない。

Mac 間 tensor-parallelism、layer-parallel の RDMA transport、distributed DSpark は v0.3.0 の対応外である。
`MXFP4` は model の quantization、`layer-parallel` は分散 topology、`DSpark` は speculative support として
別々に管理する。未文書の DS4 option を `extra_args` に追加して有効化してはならない。

## 検証とproduction gate

現在利用中の2-node環境を初期化してclean user accountを用意することは必須としない。文書gateは、repository-localなcommand/config/link/plist検証と、既存環境で取得済みの2-node actual acceptance証跡を組み合わせて判定できる。Install、login restart、cable detach/reconnectを再実行する場合は、既存model、secret、config、runtime stateを削除または上書きせず、operatorが承認した隔離pathかbackupを使う。

Production enableには`docs/compatibility/ds4-b030961.md`のsource commit、approved native binary集合、利用対象profile、distributed acceptanceがPASSであることを要求する。利用対象外profileの未検証はblockerにしない。
