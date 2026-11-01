# TP役割不明時のStandalone起動

## 目的
ケーブル切断で役割不明になった場合も、TPの共通検証後にStandaloneの起動を許可する。TPは役割不明のまま起動させず、再接続後の既存restartにより役割別artifactを検証する。

## 方針
`RuntimeDeployment::verify_tp`はcontext・量子化・capability・モデルhash・双方のrole metadata検証を全役割に適用する。role不明時だけlocal executableのpath/hash/help検証を後回しにする。`tp_command`のUnknown拒否とUnknownでcontrol planeを作らない既存境界は保持する。検証を丸ごとskipしない。

## 検証
1. 小さな隔離fixtureでUnknownの共通検証成功を追加し、旧版の `TP role unknown` 失敗を確認する。
2. 不正なmodel/capability/role metadataはUnknownでも拒否し、役割既知の場合はlocal executableを検証し続けること、UnknownでTP commandを作れないことを確認する。
3. 共通検証後のStandalone開始と、role再検出・再起動後のTP検証経路を評価する。異常時・再接続中のsecurity境界をレビューする。
4. fmt、clippy、全test、レビューを実施する。実機への配布・受入は別途実行し、成功と混同しない。
