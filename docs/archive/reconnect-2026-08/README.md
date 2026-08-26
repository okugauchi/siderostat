# reconnect 検証記録（2026-08）

Phase R0〜Q、H-01〜H-05、F-01で完了した reconnect 改善の計画・設計・実機検証文書を保管する
archive である。実装済みの現行仕様・運用手順は、次の active documents を参照する。

- [`../../spec.md`](../../spec.md): 製品 behavior と安全要件
- [`../../connection-state-machine.md`](../../connection-state-machine.md): 現行実装に基づく状態機械
- [`../../internal-operations.md`](../../internal-operations.md): 現行の運用手順
- [`../../internal-troubleshooting.md`](../../internal-troubleshooting.md): 現行の障害対応手順
- [`../../compatibility/reconnect-acceptance-2026-08-17.md`](../../compatibility/reconnect-acceptance-2026-08-17.md): 実機 acceptance と Release Candidate 判定

この archive は検証の再実行手順を削除するものではなく、完了済みの判断経緯と証跡の参照先を整理
するものである。rollback 用 binary/config/state/plist は実機環境側で保全され、repository には
runtime artifact や secret を含めない。
