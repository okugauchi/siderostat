# TP queue keepalive レビュー

対象: `src/cluster/process.rs`、`src/cluster/process/tp.rs`、関連spec/plan/実機記録。

独立レビューを実施し、修正必須のコード指摘なし。7軸とregistryを適用した。
環境overlayの適用先はTP両role共通のstartだけで、他のspawnは空overlayのまま。
所有確認、process group、起動失敗cleanup、停止・回収を既存経路と共用している。
環境全体・prompt・secretを追加出力しない。native artifactとancestryを変更しない。

重要な設計指摘として、disable変数は値0でも存在すると無効化することを確認し、TP child限定の除去を実装した。
固定環境はAppの起動policyが強制し、capability notesだけで強制されたようには扱わない。
Standalone/LPと親の環境は変更しない。

回帰テストは実childの受信値、競合値の上書き、disable除去、無関係な値と親の保持、空overlay、owned child停止を検証。
最初のREDは`999|disabled|retained`に対して`100||retained`を期待して失敗することを確認した。
owned fixtureはmacOSの`/bin/sh`→`/bin/bash`再execによるIdentityMismatchを観測したため、直接`/bin/bash`を使う。
本番のidentity検査を緩和する変更は行っていない。

対象環境試験、fmt、workspace全target/all-feature clippy、workspace全target/all-feature試験
（1,064件、68 suites、exit 0）、diff checkに成功。独立再レビューでも追加指摘なし。
署名済みbuild35の通常runtime受入は実行後に追記する。
