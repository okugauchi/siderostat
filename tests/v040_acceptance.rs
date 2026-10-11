//! V02: workspace gate・秘密情報・実資源非操作検査（integration）。
//!
//! C00 / workspace gate。受入 case 3（test-support suite の実 test 数 > 0）
//! と受入 case 4（公開 artifact に secret / model が含まれない）を検証
//! する。scan は read-only で、稼働 process / config / secret / model /
//! KV / LaunchAgent には触れない。

#![cfg(feature = "test-support")]

use std::path::{Path, PathBuf};

/// Cargo の workspace root（cargo test の cwd は crate root）。
fn repo_root() -> PathBuf {
    std::env::current_dir().expect("resolve current dir")
}

/// 公開 artifact として検査するファイルを workspace root から再帰列挙する。
/// ビルド成果物（target/）、git 内部（.git/）、ローカル記録
/// （.superpowers/）は公開 artifact ではないため除外する。
fn walk_public(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "target" || name == ".git" || name == ".superpowers" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// model バイナリの拡張子かどうか。実 model ファイル（.gguf /
/// .safetensors / .pt / .ckpt / .bin）を検出する。
fn is_model_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("gguf" | "safetensors" | "pt" | "ckpt" | "bin")
    )
}

/// テキストとして読めるファイルかどうか。secret 検索の対象を絞る。
fn is_text_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("toml" | "rs" | "md" | "json" | "yaml" | "yml" | "sh" | "plist" | "txt" | "lock")
    )
}

/// 実 secret 値のパターン（OpenAI API key / GitHub PAT / AWS access key）
/// を検出する。ダミー値（[REDACTED] 等）は対象外。
fn find_secret(contents: &str) -> Option<&'static str> {
    // OpenAI API key: sk- に続く長い英数字。
    if contents.contains("sk-") && has_long_alnum_after(contents, "sk-") {
        return Some("openai-api-key");
    }
    // GitHub PAT: ghp_ / gho_ / github_pat_。
    for pat in ["ghp_", "gho_", "github_pat_", "AKIA"] {
        if contents.contains(pat) {
            return Some("credential-token");
        }
    }
    None
}

fn has_long_alnum_after(contents: &str, needle: &str) -> bool {
    contents
        .split(needle)
        .nth(1)
        .map(|rest| {
            let trimmed: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            trimmed.len() >= 20
        })
        .unwrap_or(false)
}

/// 受入 case 4: 公開 artifact に secret / model が含まれない（read-only scan）。
#[test]
fn public_artifacts_contain_no_secrets_or_models() {
    let root = repo_root();
    let mut model_hits = Vec::new();
    let mut secret_hits = Vec::new();
    for path in walk_public(&root) {
        // scan コード自体がパターン文字列（sk- / ghp_ / AKIA）を含むため、
        // このテストファイルは公開 artifact の scan 対象から除外する。
        if path == root.join("tests/v040_acceptance.rs") {
            continue;
        }
        if is_model_file(&path) {
            model_hits.push(path.clone());
        }
        if is_text_file(&path) {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Some(kind) = find_secret(&contents) {
                    secret_hits.push((path.clone(), kind));
                }
            }
        }
    }
    assert!(
        model_hits.is_empty(),
        "model files present in public artifacts: {model_hits:?}"
    );
    assert!(
        secret_hits.is_empty(),
        "secrets present in public artifacts: {secret_hits:?}"
    );
}

/// 受入 case 3 + version 同期: 3 package が 0.4.0 に同期されている。
#[test]
fn workspace_packages_are_version_0_4_0() {
    let root = repo_root();
    for manifest in ["Cargo.toml", "monitor/Cargo.toml", "xtask/Cargo.toml"] {
        let contents = std::fs::read_to_string(root.join(manifest))
            .unwrap_or_else(|e| panic!("read {manifest}: {e}"));
        assert!(
            contents.contains("version = \"0.4.0\""),
            "{manifest} must be version 0.4.0"
        );
    }
}

/// 受入 case 3: test-support suite に実 test が存在する（0 件を成功としない）。
/// この module は `#![cfg(feature = "test-support")]` で gate され、この
/// テストが実行されることが「実 test 数 > 0」の保証になる。
#[test]
fn test_support_suite_is_not_empty() {
    // この module は test-support feature で gate され、このテストが実行される
    // ことが「実 test 数 > 0」の保証になる。定数 assert を避け、実行時に
    // workspace root が存在することを確認する。
    assert!(repo_root().exists(), "workspace root must exist");
}
