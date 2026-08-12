# macOS user service draft

`siderostat`だけを1つのLaunchAgentとして登録します。DS4 childはproxyが所有・検証・停止するため、`ds4-server`用のplistや同じlisten portを使う別jobを作成しないでください。

## Install前の準備

Exampleを作業用fileへcopyし、次の全pathを実在するabsolute pathへ置換します。

- `/usr/local/bin/siderostat`: installしたproxy binary
- `/Users/USERNAME/Library/Application Support/siderostat/config.toml`: node別config
- `/Users/USERNAME/Library/Logs/siderostat/`: stdout/stderr directory

Tokenやsecret値をplist、`EnvironmentVariables`、command lineへ書きません。Configにはpermission `0600`のsecret file pathだけを設定します。

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

`ProgramArguments`の各要素がabsolute pathまたは固定subcommandであり、placeholderが残っていないことを登録前に確認します。

## GUIドメイン必須

このplistには `LimitLoadToSessionType = Aqua` を指定しています。デスクトップ通知
(`osascript display notification`) はユーザーのAquaセッション内のNotification Centerで
表示されるため、LaunchAgentを `gui/<uid>` ドメインに載せる必要があります。

- `system/` (LaunchDaemon) や `user/<uid>` ドメインへ誤ってロードすると、ロード自体が
  失敗するか、通知が黙って表示されません。
- 登録後、次のコマンドで `gui/<uid>` ドメインに載っていることを確認します。

```sh
launchctl print "gui/$(id -u)/local.siderostat.runtime"
```

`state` が `running` であることと、`LimitLoadToSessionType` が `Aqua` として適用されて
いることを確認してください。ログアウト時は `gui/<uid>` ドメインごとagentが停止するため、
通知は出ません (想定どおりです)。

## Verification

```sh
launchctl print "gui/$(id -u)/local.siderostat.runtime"
pgrep -alf siderostat
pgrep -alf ds4-server
curl --fail --silent http://127.0.0.1:18081/healthz
```

次を確認します。

1. Login後にproxyが1 processだけ起動する。
2. `launchctl kickstart -k`後もproxyが1 process、DS4 childが最大1 processである。
3. Proxyを終了すると10秒以上のthrottleを伴って再起動し、orphan DS4 childを残さない。
4. `launchctl print-disabled "gui/$(id -u)"`と`$HOME/Library/LaunchAgents`に、同じbinary/portやDS4 childを所有する別jobがない。

## Uninstall

先にjobを停止してからplistを移動します。Model、KV cache、secret、runtime stateは自動削除しません。

```sh
launchctl bootout "gui/$(id -u)/local.siderostat.runtime"
mv "$HOME/Library/LaunchAgents/local.siderostat.runtime.plist" "$HOME/Library/LaunchAgents/local.siderostat.runtime.plist.disabled"
```
