# siderostat 導入ガイド

この文書は、`docs/spec.md` と `docs/compatibility/ds4-b7e9f00.md` を根拠に、既存DS4環境がない状態から2-nodeの`DistributedReady`へ到達する手順を定める。実DS4 binaryとmodelを使う手順はoperatorが実行し、本guideはその手順と記録方法を定義する。

## 対象と前提

- 対象platform: macOS 26系、Apple Silicon。
- 対象topology: Thunderbolt Bridgeで直結した2 node。M4 Max 128GB coordinator、M5 Max 128GB worker、Thunderbolt 5直結を基準とする（spec第32.5節）。
- Role addressは`bridge0`のIPv4から決定する。`10.99.0.1`がcoordinator、`10.99.0.2`がworker、その他/未設定/競合はunknown（spec第13.2節）。Roleはconfigで指定しない。
- Proxyは`rewrite/mode-aware`branchでbuildする。

実DS4 source commit `b7e9f00`は仕様書記載の短縮SHAであり、full SHAは未確認である。利用対象profileとdistributed acceptanceの結果は`docs/compatibility/ds4-b7e9f00.md`で確認する。Source baselineの不一致はfinal release acceptanceまでに解決し、model配布条件や未確認URLを推測しない。

## 1. 前提、DS4 checkout/build、digest記録

対象Macで次の前提を確認する。

- macOS 26系、Apple Silicon。
- Rust stable（edition 2024）が利用可能。
- Thunderbolt Bridge `bridge0`がSystem Configurationのnetwork serviceとして存在し、enabledである。

DS4 binaryはoperatorの既知のsourceから取得する。Repository URLや配布条件は推測しない。対象commitを`b7e9f00`へ固定する。

```sh
DS4_CHECKOUT="/absolute/path/to/ds4-checkout"
git -C "$DS4_CHECKOUT" rev-parse HEAD
git -C "$DS4_CHECKOUT" checkout b7e9f00
git -C "$DS4_CHECKOUT" rev-parse HEAD
```

`rev-parse HEAD`が仕様書記載のexpected baseline `b7e9f00`に対応しない場合、baselineを更新せず作業を停止する。対応する場合のみ続行する。

DS4 binaryをbuildし、digestを記録する。Build手順はoperatorの既知のsourceに従う。Binary pathとSHA-256を`docs/compatibility/ds4-b7e9f00.md`へ追記する。

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

Supervisorが使用予定のoptionがCLI helpに存在することを確認する（`docs/compatibility/ds4-b7e9f00.md`のCLI compatibility一覧）。`--version`はunknown optionとして拒否されるため、binary単体からcommitは確定できない。

## 2. Model選択/取得/checksum/配置

Standalone profileとdistributed profileは独立した設定である（spec第14.1節）。

- Standalone model variant: `q2`、`q2-q4`、`mxfp4`。
- Standalone residency: `resident`または`ssd-streaming`。
- Production Standalone: resident + DSpark support GGUF（現行DS4ではSSD streamingおよびDistributedとの併用不可）。
- Distributed profile: `distributed-mxfp4`。

Modelはoperatorの既知のsourceから取得する。URLや配布条件は推測しない。取得後、両nodeでchecksumとsizeを記録する。

```sh
MODEL_PATH="/absolute/path/to/model.gguf"
shasum -a 256 "$MODEL_PATH"
ls -l "$MODEL_PATH"
```

Distributed MXFP4は両nodeでcontent SHA-256を一致させる（spec第14.2節）。現行配布では約156GBのMXFP4 GGUFを両nodeへ配置する（spec第14.2節）。不一致ならMXFP4 promotionを拒否する（spec第15.3節）。DSpark support GGUFも各nodeでchecksum/sizeを記録し、そのnodeのStandalone manifestへ設定する。DS4 binaryはnode別digestを記録し、byte-for-byte一致ではなく、actual acceptance済みdigestだけを両manifestの同一 `compatible_ds4_binary_sha256` 集合へ昇順で記載する。未知rebuildを自動追加しない。

Modelはcanonical absolute pathで指定し、書換可能なsymlinkを使わない（spec第14.2節、第22.3節）。配置先は`siderostat.example.toml`のplaceholder pathへ合わせる。

Manifestは`docs/spec.md`第15.1節のschemaに従い、standaloneとdistributedのdigest情報をJSONで作る。DSpark有効なStandalone manifestには`dspark_enabled`、support digest/size、confidence、strictを記録する。起動時に実support fileとtyped configへ一致しなければchildはspawnされない。Standalone manifestはpeer間compatibilityの比較対象にしない（spec第15.3節）。

## 3. Resident/SSD streaming standalone smoke

実DS4とmodelを使うため、この手順はoperator gateである。`docs/compatibility/ds4-b7e9f00.md`のModel/profile matrixを基準とし、次の対象を確認する。

- Q2-Q4 resident standaloneでrequest成功。Q2は対応するfull standalone modelを利用する構成だけで追加確認する。
- Q2-Q4 resident + DSparkでsanitized `dspark-activated` log、HTTP readiness、short requestを確認する。
- Q2-Q4 SSD streaming standaloneでrequest成功。
- MXFP4 SSD streaming standaloneでrequest成功。対象DS4 build/Metal backendで未確認の場合は、そのprofileだけをproduction enable不可とする。
- `ssd-streaming`はDS4の`--ssd-streaming`を意味し、model variantと混同しない（spec第14.1節）。`residency="resident"`ではSSD streaming optionを生成しない。
- HTTP readiness、short prompt、streaming、memory/startup timeを確認する。

実測結果を`docs/compatibility/ds4-b7e9f00.md`のModel/profile matrixへ追記し、productionで利用するprofileのstatusをPASSへ更新してから次へ進む。利用対象外のQ2 resident、Q2 SSD streaming、MXFP4 residentはPhase 6 / release gateにしない。

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

Binaryは`target/release/siderostat`である。LaunchAgentの`ProgramArguments`が参照する`/usr/local/bin/siderostat`へinstallする。`/usr/local/bin`への書き込みに管理者権限が必要な環境では、ownerとmodeを固定する。

```sh
sudo install -d -m 0755 /usr/local/bin
sudo install -m 0755 target/release/siderostat /usr/local/bin/siderostat
/usr/local/bin/siderostat --help
```

### Secret file

Secret/token fileは各32 bytes以上、mode `0600`、相互に異なるpathで配置する（spec第22.3節、第32.5節）。Control secretとpeer proxy tokenは、それぞれclusterの両nodeで同じ値が必要である。Admin tokenはnode-localとする。Control secret、peer proxy token、admin token間でfileまたは値を流用しない。

まずcoordinator上で共有する2値とcoordinatorのadmin tokenを生成する。`umask 077`により生成時からownerだけが読み書きできる。

```sh
SECRET_DIR="$HOME/Library/Application Support/siderostat/secrets"
umask 077
mkdir -p "$SECRET_DIR"
openssl rand -out "$SECRET_DIR/cluster-control.key" 32
openssl rand -out "$SECRET_DIR/peer-proxy.key" 32
openssl rand -out "$SECRET_DIR/admin.key" 32
chmod 600 "$SECRET_DIR/cluster-control.key" "$SECRET_DIR/peer-proxy.key" "$SECRET_DIR/admin.key"
```

`cluster-control.key`と`peer-proxy.key`を、operatorが承認した暗号化済み媒体または同等の安全な経路でworkerへ移す。Shell history、clipboard manager、repository、plist、command lineにsecret値そのものを記録しない。Workerでは共有2 fileを同じfilenameで`SECRET_DIR`へ配置し、admin tokenだけをworker上で新規生成する。

```sh
SECRET_DIR="$HOME/Library/Application Support/siderostat/secrets"
SHARED_SECRET_SOURCE="/Volumes/OPERATOR-APPROVED-ENCRYPTED-MEDIA"
umask 077
mkdir -p "$SECRET_DIR"
install -m 0600 "$SHARED_SECRET_SOURCE/cluster-control.key" "$SECRET_DIR/cluster-control.key"
install -m 0600 "$SHARED_SECRET_SOURCE/peer-proxy.key" "$SECRET_DIR/peer-proxy.key"
openssl rand -out "$SECRET_DIR/admin.key" 32
chmod 600 "$SECRET_DIR/cluster-control.key" "$SECRET_DIR/peer-proxy.key" "$SECRET_DIR/admin.key"
```

両nodeの共有2 fileがbyte-for-byteで一致し、各node内の3 fileがそれぞれ異なることを安全な経路上で確認する。Digestやsecret値はdocumentation evidenceに保存しない。

### Config

`siderostat.example.toml`をnode別の作業fileへcopyし、全ての`PLACEHOLDER`を実在するabsolute pathへ置換する。`$HOME`と`~/`だけを展開する（spec第22.1節）。Worker nodeは`cluster.node_id`とnode固有pathだけを変更する。Roleはinterface addressから決定し、設定で直接指定しない（spec第22.2節）。

```sh
cp siderostat.example.toml "$HOME/Library/Application Support/siderostat/config.toml"
```

置換対象：

- `ds4.binary`の`PLACEHOLDER-ds4-server`。
- `ds4.dspark.support_model`の`PLACEHOLDER-dspark-support-0731.gguf`。
- `ds4.standalone.model`と`ds4.mxfp4.model`の`PLACEHOLDER-*.gguf`。
- Secret fileの`PLACEHOLDER-*-cluster-control.key`、`PLACEHOLDER-*-peer-proxy.key`、`PLACEHOLDER-*-admin.key`。両nodeで共有するcontrol/peerの値とnode-localのadmin値を参照する。
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

MXFP4 promotionにはdeployment matchと実HELLOとcomplete routeが必須（spec第33.3節）。まずfingerprintを実行する。

```sh
siderostat cluster fingerprint --profile standalone
siderostat cluster fingerprint --profile distributed
```

Fingerprint jobは202 + job IDを返し、同一profileの同時jobを拒否する（spec第23.1節）。数百GBを読む処理をHTTP handler内で同期実行しない（spec第15.2節）。

Binary/model/checkpoint/context/layer split/wire schema/argv profileが一致する場合だけ、実HELLOを受けてpromotionする（spec第15.3節）。`cluster promote`はHELLO/compatibility条件を迂回しない（spec第23.2節）。実HELLOなしでpromotionしない。

```sh
siderostat cluster promote
```

Cluster-wide drain後にDS4を停止し、coordinator MXFP4起動（`--debug`）、worker registered、complete route readyでDistributedReadyへ入る（spec第18.3節）。HTTP listeningだけでDistributedReadyにしない。

Promotion完了後は両nodeで`mode=distributed-mxfp4`、`state=distributed-ready`、`ready=true`を確認する。Fingerprint commandは受付時にjob ID付きJSONを返す。Compatibility不一致、HELLO timeout、route incompleteのいずれかがある場合は`distributed-ready`を期待せず、[`docs/troubleshooting.md`](troubleshooting.md)に従う。

### LaunchAgent

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
/usr/libexec/PlistBuddy -c "Set :StandardOutPath $HOME/Library/Logs/siderostat/stdout.log" "$PLIST"
/usr/libexec/PlistBuddy -c "Set :StandardErrorPath $HOME/Library/Logs/siderostat/stderr.log" "$PLIST"
plutil -lint "$PLIST"
if grep -Eq 'USERNAME|PLACEHOLDER' "$PLIST"; then echo "unresolved LaunchAgent placeholder" >&2; exit 1; fi
launchctl bootstrap "gui/$(id -u)" "$PLIST"
launchctl kickstart -k "gui/$(id -u)/local.siderostat.runtime"
```

Verification（`contrib/launchd/README.md`）:

1. Login後にproxyが1 processだけ起動する。
2. `launchctl kickstart -k`後もproxyが1 process、DS4 childが最大1 processである。
3. Proxyを終了すると10秒以上のthrottleを伴って再起動し、orphan DS4 childを残さない。
4. `launchctl print-disabled "gui/$(id -u)"`と`$HOME/Library/LaunchAgents`に、同じbinary/portやDS4 childを所有する別jobがない。

Login起動、proxy restart、no duplicate childはoperator gateである（GUI user session変更が必要）。

### Recovery

- Thunderbolt cable detachで両nodeがlocal standaloneへ復帰する。Address/route喪失またはlease失効でSolo Standaloneへ収束する（spec第18.5節）。
- Route loss grace（既定15秒）経過後はPaired Standaloneへ復帰する（spec第18.4節）。
- Cable再接続後、debounceしたpeer discoveryが再評価され、Paired Standaloneへ、次いでMXFP4再昇格する（spec第32.5節）。
- 単一eventを「peer接続済み」「peer切断済み」と解釈しない（spec第13.5節）。

### Upgrade

DS4 update時は`docs/spec.md`第36節のcompatibility trackingに従う。

- Verified DS4 commit、binary digest、wire/log fixture digest、recognized event、tested model/topology、dateを`docs/compatibility/ds4-b7e9f00.md`へ記録する。
- `ds4_distributed.c/.h`、model ID/name/quant/layer API、server signal handling、server/distributed log、GGUF/checkpoint、CLI option、distributed QAを確認する。
- Unknown changeではpromotionをfail closedにする。

### Rollback

- Legacy config v1の`backends`、`routing`、`affinity`、`heartbeat`、`active_probe`、`cooldown`、SQLite pathは廃止されている（spec第22.4節）。Unknown/legacy fieldを黙って無視しない。
- 旧affinity databaseを自動削除しない。
- 新configは旧configと分離し、`schema_version == 2`の作業fileを保持する。
- Binary rollbackは直前のbinaryを残し、standalone readinessを確認してから行う。Upgrade後にrollbackし、再度upgradeする（plan P7-02）。

### Uninstall

`contrib/launchd/README.md`に従う。先にjobを停止してからplistを移動する。Model、KV cache、secret、runtime stateは自動削除しない。

```sh
launchctl bootout "gui/$(id -u)/local.siderostat.runtime"
mv "$HOME/Library/LaunchAgents/local.siderostat.runtime.plist" "$HOME/Library/LaunchAgents/local.siderostat.runtime.plist.disabled"
```

## 検証とproduction gate

現在利用中の2-node環境を初期化してclean user accountを用意することは必須としない。文書gateは、repository-localなcommand/config/link/plist検証と、既存環境で取得済みの2-node actual acceptance証跡を組み合わせて判定できる。Install、login restart、cable detach/reconnectを再実行する場合は、既存model、secret、config、runtime stateを削除または上書きせず、operatorが承認した隔離pathかbackupを使う。

Production enableには`docs/compatibility/ds4-b7e9f00.md`の利用対象profileとdistributed acceptanceがPASSであることを要求する。利用対象外profileの未検証はblockerにしない。DS4 source baselineの不一致は文書gateと分離し、final release acceptanceで解決する。
