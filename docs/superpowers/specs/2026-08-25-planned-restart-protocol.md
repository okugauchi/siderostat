# 分散モードの計画再起動プロトコル

## 背景

分散 `DistributedReady` 中に `/admin/restart` が実行されると、ローカルの
distributed child だけが停止し、相手ノードには再起動の意図が伝わらない。その結果、
相手ノードが control lease の失効を通常の PeerLost と解釈し、Solo 復旧・自動 Pair・
自動 Promote を再起動中に繰り返す。

## 目的

- 計画された再起動と、予期しない peer 消失を制御上区別する。
- 再起動中は peer-loss recovery、route-loss demotion、automatic pair/promote を抑止する。
- 現在の coordinator 主導の再起動で、相手 worker を Solo へ落とさず Paired 待機にする。
- 再起動後の新しい Pair を復帰点として抑止を解除し、DistributedReady へ一度だけ再収束させる。
- drain timeout、control 通信失敗、child identity mismatch の場合は再起動を完了扱いにせず、再試行可能なエラーにする。

## プロトコル

1. 再起動要求を受けたノードは、ローカルの planned-restart gate を立てる。
2. coordinator は authenticated control plane の `prepare-restart` を worker に送り、worker
   は gate を立てて ACK を返す。これは child をまだ停止しない準備段階である。
3. ローカル admission を block し、in-flight request を drain する。失敗時はローカルの
   gate を解除する。
4. coordinator は通常の recovery demotion を呼ばず、まず自分の distributed child を
   lifecycle owner 経由で停止する。停止に成功した後、worker へ `Demote` を送り、worker
   child を停止してから両 node を `PairedStandaloneReady` へ遷移させる。planned-restart
   gate により、lease が一時的に失効しても worker は PeerLost 復旧を開始しない。coordinator
   child の identity mismatch／停止失敗時はこの状態遷移を開始せず、local target と admission
   を復元して再試行可能なエラーにする。
5. server loop は既存の正常終了処理を通り、LaunchAgent の KeepAlive に再起動を任せる。
6. worker は再起動後の新しい `Pair` を受信した時点で gate を解除する。通常の reconcile と
   auto-promote はこの Pair 後にのみ再開する。

`prepare-restart` は同じ control generation / request ID について冪等である。準備後に
ローカル処理が失敗した場合は `cancel-restart` を best effort で送り、peer 側に残った
gate を解除する。

## 適用範囲

今回の実装対象は、現在の製品経路である coordinator からの distributed graceful restart
である。通常のプロセス突然終了は従来どおり PeerLost recovery を行う。worker からの
独立した管理再起動は別タスクとして扱い、今回の command はその将来拡張を妨げないよう
authenticated control command として定義する。

## 受け入れ条件

- planned restart 中に `PeerLost` による Solo 遷移が発生しない。
- planned restart 中に automatic Pair / Promote が開始されない。
- 再起動後は Pair を契機に gate が解除され、通常の DistributedReady 復帰が可能である。
- 予期しない peer 消失では既存の Solo 復旧が維持される。
- 重複した prepare/cancel は安全に処理される。
- 単体テスト、全 Rust テスト、clippy、format、diff check が通る。
