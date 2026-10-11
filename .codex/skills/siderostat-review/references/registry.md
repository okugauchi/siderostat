# プロジェクト固有レビュー観点レジストリ（成長）

個別の開発項目のレビューで発見された、特定機能に閉じない教訓を蓄積する。新しい開発項目のレビュー時にはここを読み、適用する。新たに汎用化できる観点が見つかったら、このファイルに追記する（出典の開発項目と日付を添える）。

## 軸2: 影響範囲の封じ込め
- 新モード（dry-run 等）が実プロセスの起動/停止/再起動を絶対に実行しないこと（startup cleanup、restart reconcile、spawn / SIGTERM / SIGKILL、persist / load StateStore をスキップ）
- 新モードの supervisor が実 child を持たず `child_identity` が None を返すこと
- 通常経路（new()）が新モードに誤って混入しないこと
- 新フラグが既定値で有効にならないこと
- 出典: feature/cluster-dry-run（2026-09-05）

## 軸3: 抽象化・シミュレーションの整合性
- シミュレートされた通信フレーム（HELLO）が本番の parse / rendezvous 検証を通ること
- route probe などの合成観測が本番の前提条件（phase・peer_present）と一致すること。phase 遷移（WorkerReady → Drained 等）を考慮して `wait_ready` / `wait_route_loss` が正しく発火すること
- 合成データへの参照が Weak 参照等で runtime の lifetime を延長しないこと
- 新モードの協調者 lifecycle が診断（children.*）に反映されること
- 出典: feature/cluster-dry-run（2026-09-05）

## 軸5: Test evidence
- 新モードの統合テストは、実 control HTTP で pair → promote → demote を通し、実プロセス無し（pid=None）を検証すること
- 同一ホストに複数ノードを同居させるテストでは、peer の制御ポートを明示分離できること
- 出典: feature/cluster-dry-run（2026-09-05）
