# TP queue keepalive Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 上流750msを維持し、managed TP childだけに100ms queue keepaliveを指定する。

**Architecture:** owned-child spawnの環境overlayを追加し、既存spawnは空overlayとする。TP supervisorだけが固定の設定・除去を渡す。

**Tech Stack:** Rust / Tokio / macOS / upstream ds4 Metal。

**Spec:** `docs/superpowers/specs/2026-10-28-tp-queue-keepalive-design.md`

## Global Constraints

- 元のnative binary、モデル、750ms、Standalone/LP、親環境を変更しない。
- 環境全体・prompt・secretを記録しない。
- process ownership、起動失敗cleanup、停止処理は既存経路を共用する。

## Review Focus

- disable変数は存在判定なので0でも除去する。
- 空overlayはenv_clearではなく従来の継承である。
- 両TP roleへ同じ設定を適用し、他roleへ適用しない。
- 実binaryでidleを含む試験が成功してから通常運用の受入へ進む。
- native source ancestryやcapabilityの保証範囲を偽らない。

### Task 1: child環境とTP起動の接続

Files: `src/cluster/process.rs`、`src/cluster/process/tp.rs`。

- [x] child commandへ競合値を入れた実行テストと、空overlayの回帰テストを追加しREDを確認する。
- [x] 環境overlay内部spawnとTP両roleへの指定・限定ログを実装する。
- [x] 対象試験、既存owned-child/TP試験、fmt/clippy/workspace試験を実行する。
- [x] 独立レビューの指摘を解消し、sourceと運用文書をcommitする。

### Task 2: build35と実機受入

- [x] 元binary・100ms・750msの28要求試験を確認し、条件と結果を保存する。
- [x] build35を署名・payload検証して両ノードへ配置する。
- [x] 通常TPの生成・SSE・session維持・Standalone復帰・再TP・peer喪失後の復帰を確認する。
- [x] 合否と未実施項目、揮発するOS設定を記録する。

Ruling: 実装は既存source branch上で行い、配布は既存detached worktreeを検証済みcommitへ更新して作成する。既存の実機設定とmodelを複製しない。
Ruling: `/bin/sh`が`/bin/bash`へ再execするfixtureでは所有確認を緩めず、試験だけ直接`/bin/bash`を使う。

実機結果: 通常750ms・TP child100msでsession 9/10/11の生成・SSE、idle30秒、Standalone往復、生成中worker runtime SIGTERM後の自動TP再形成に成功。物理ケーブル/OS再起動/長期運転は未実施。詳細は実機記録を参照。
