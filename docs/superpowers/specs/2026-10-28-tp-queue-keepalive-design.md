# TP子プロセスのGPU queue keepalive

目的: M5 Max / M4 Max各128GB、V4.1 Flash 1017 Q2の通常TPを、上流の750ms期限を延長せず安定化する。

## 根拠と境界

通常起動と上流直接起動の双方で、idle後にgate timeoutを再現した。GPU pollがCPU期限より先に尽きるケースもある。
共有event待ちだけでもCPU側750ms超過があり、GPU処理開始遅延の抑制が必要。
診断ログではmain queueへ約1.0〜1.2秒ごとにkeepaliveを投入していても、軽いCBのdriver処理に1秒以上かかった。
100ms設定では、Startupと推論中±1秒を除いたStudio idleサンプル878件がすべて0.3ms以下となった。
既定間隔では193件中112件が100msを超え、最大1465.9msだった。単一原因・全モデルの保証とは扱わない。

上流の既存 `DS4_METAL_QUEUE_KEEPALIVE_MS=100` をmanaged TP childだけへ指定する。
`DS4_METAL_DISABLE_QUEUE_KEEPALIVE` は値0でも無効化となるため、TP childからだけ除去する。
親環境、Standalone、LP、native source/binary/help、model、750ms期限、manifest schemaは変更しない。
Source改変を伴う診断版は配布しない。5000msの診断overrideは採用しない。

100ms化は小さいGPU処理の投入頻度を増やす。電力影響ゼロとは主張しない。
GPU poll枯渇時の安全性は上流側の別修正候補であり、この設定で全てのGPU遅延を解決したとは扱わない。

## 実装

共通owned-child spawnへ、child限定の環境設定・除去を指定できる内部経路を追加する。
既存spawnは空の設定を渡し、従来の継承・所有確認・process group・cleanupを共用する。
TP supervisorの両role共通の起動箇所だけが、100ms指定とdisable除去を利用する。
環境全体をログ出力せず、role、session、managed intervalとdisable除去方針だけを構造化記録する。

## 受入

child用commandへ競合値を設定し、実際に受け取る値、無関係な設定の保持、親環境の不変を確認する。
空overlayでは既存の環境設定を保持する。環境テストでglobal envを変更しない。
元の上流binary・750ms・通常pollのまま、待機、連続要求、非stream/SSEを確認する。
更新Appの通常起動でwarm-up、両公開API、session維持、Standalone復帰、再TP、peer停止後の復帰を確認する。
検証済みモデルのcapability notesには実施条件を記録するが、notesを環境強制の仕組みとは扱わない。
