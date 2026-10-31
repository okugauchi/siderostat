# 停止進捗レビュー

対象はruntime終了順序、admin操作、Monitor表示、TP役割変更と運用文書。汎用7軸とregistryを確認した。
独立レビューで、admin異常時のpublic drain抜け、abortしたtaskのjoin、既存admin jobとの競合、job永久待機、
child stop失敗時のowner喪失、retry重複と旧通知、遅延したmanual restartのphase競合を指摘された。
修正後の再レビューでは追加の重大指摘なし。個別の競合・HTTP監視・失敗時挙動に回帰試験を追加した。

注意: 成功/失敗の短いphaseはpoll間隔で見落とされ得る。API喪失後はofflineで回収成否不明。
実ケーブル切断・生成中・OSによる停止は未受入。public drainは無期限で、現行LaunchAgentの停止期限5秒との
不整合が残るため、OSによる停止時のowner保持を保証しない。設定値や署名済みAppはこの変更では更新しない。
