# Public User Documentation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** README.md を英語の正本、README.ja.md と公開ガイドの `.ja.md` を日本語翻訳版とし、README から辿る文書をエンドユーザー向けに限定する。

**Architecture:** README と3つの公開ガイド（installation、operations、troubleshooting）を英語正本として管理し、それぞれに同じ構成の `.ja.md` を置く。既存の仕様・開発・配布文書は内部参照用として保持するが、公開READMEの関連リンクには掲載しない。各言語のREADMEは同じ公開ガイドの同言語版へリンクする。

**Tech Stack:** Markdown、既存のローカルリンク、Siderostat v0.3.0 の確定したユーザー向け挙動。

**Spec:** ユーザー指示（2026-08-26）、`README.md`、`docs/implementation-plan-v0.3.0.md` R-01、`docs/distribution/macos-app-bundle-pkg-spec.md`。

## Global Constraints

- `README.md` と英語の公開ガイドを正本とし、日本語版は対応する `.ja.md` とする。
- README から辿る公開文書には、内部のcommit、digest、CI、テストfixture、ソースファイル、開発用 `cargo xtask`、手動LaunchAgent操作を記載しない。
- v0.3.0 の公式導入はソース checkout からの `cargo xtask install --start` とする。事前ビルド済み
  DMG/pkg、署名、公証、Login Items / Background Items の自動設定を公式配布条件にしない。
- 公開文書は、ユーザーがFinder、System Settings、Siderostatのメニューで完了できる手順を優先する。
- 既存の開発・仕様文書は削除せず、READMEの公開導線から外す。

---

### Task 1: 公開文書の英語正本を作る

**Files:**
- Modify: `README.md`
- Modify: `docs/installation.md`
- Modify: `docs/operations.md`
- Modify: `docs/troubleshooting.md`

- [x] README の関連文書リンクを installation、operations、troubleshooting に限定し、開発者向け・仕様向けリンクを削除する。
- [x] 各英語文書を、通常インストール、日常操作、ユーザーが実施できるトラブル対応だけを含む内容へ整理する。
- [x] 内部の実装識別子、開発用コマンド、テスト・CI・digest・commit情報、手動LaunchAgent操作を公開文書から除外する。

### Task 2: 日本語翻訳版を同期する

**Files:**
- Modify: `README.ja.md`
- Create: `docs/installation.ja.md`
- Create: `docs/operations.ja.md`
- Create: `docs/troubleshooting.ja.md`

- [x] README.md と同じ章構成・リンク対象を README.ja.md に反映する。
- [x] 3つの英語公開ガイドを、意味と安全上の注意を保った日本語へ翻訳する。
- [x] 日本語版からは日本語ガイド、日本語READMEからは英語正本へ移動できるようリンクする。

### Task 3: 公開導線と文書品質を検証する

**Files:**
- Verify: `README.md`, `README.ja.md`, `docs/installation.md`, `docs/installation.ja.md`,
  `docs/operations.md`, `docs/operations.ja.md`, `docs/troubleshooting.md`,
  `docs/troubleshooting.ja.md`

- [x] 英語・日本語の各文書に対応するローカルリンクが存在することを確認する。
- [x] 公開文書に開発用語や内部導線が残っていないことを検索で確認する。
- [x] `git diff --check` を実行し、翻訳版と正本の章見出し・リンク対象の対応を確認する。
- [x] R-01 のユーザーレビュー対象を、インストール、Background Items、recovery文言・安全警告の3点として計画書に記録する。
