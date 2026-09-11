//! W09 — 対象 Codex parser replay。受入 case 3（全 fixture → 対象 parser
//! 受理）を検証する。W09。
//!
//! レビュー重点「自作 parser で自作 SSE を受理するだけの循環試験は不可」に
//! 対応するため、本テストは Codex の実 parser（`codex_protocol::ResponseItem`）
//! で fixture を deserialize する replay binary を外部プロセスとして呼び、
//! 全 fixture の各 item が実 parser で受理されることを検証する。W09。
//!
//! replay binary は隔離環境（A03 の対象 Codex source）に置かれた
//! `codex-replay` である。binary が見つからない場合は環境不足として明示的に
//! 失敗する（0 件を成功と扱わない）。W09。
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。W09。
use std::path::PathBuf;
use std::process::Command;

/// codex-replay binary の既定パス（隔離環境の A03 対象 Codex workspace）。
/// 環境変数 CODEX_REPLAY_BIN で上書き可能。W09。
fn replay_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CODEX_REPLAY_BIN") {
        return PathBuf::from(path);
    }
    PathBuf::from("/Users/o/LLM/codex-v040-tmp/codex-rs/target/debug/codex-replay")
}

/// replay fixture（`replay-*.json`）を列挙する。W09。
///
/// wire-profile.json は A03 の wire 文書（items 配列形式でない）であり、
/// replay の対象外。replay 対象は本 task が用意した items 配列形式の
/// fixture（replay- 接頭辞）に限定する。W09。
fn fixtures() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/responses/v040");
    let mut list: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixtures dir must exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("replay-") && n.ends_with(".json"))
        })
        .collect();
    list.sort();
    list
}

/// 受入 case 3: 全 fixture → 対象 parser 受理。W09。
///
/// 実 Codex parser（codex-replay binary）で全 fixture の各 item を
/// deserialize し、全て受理されることを検証する。循環試験を避けるため、
/// 本テストは自作 parser を使わず、外部の実 parser を呼ぶ。W09。
#[test]
fn w09_all_fixtures_accepted_by_real_codex_parser() {
    let binary = replay_binary();
    assert!(
        binary.exists(),
        "codex-replay binary not found at {} — A03 の対象 Codex parser を隔離環境で \
         build してから再実行してください (CODEX_REPLAY_BIN で上書き可)",
        binary.display()
    );

    let fixtures = fixtures();
    // 0 件を成功と扱わない。W09。
    assert!(
        !fixtures.is_empty(),
        "no fixtures under tests/fixtures/responses/v040"
    );

    let mut args: Vec<std::ffi::OsString> = Vec::new();
    for f in &fixtures {
        args.push(f.as_os_str().to_owned());
    }
    let output = Command::new(&binary)
        .args(&args)
        .output()
        .expect("codex-replay must run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "codex-replay failed (exit {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("ALL_FIXTURES_ACCEPTED"),
        "expected ALL_FIXTURES_ACCEPTED, got:\n{stdout}\n{stderr}"
    );
    // 受理された item 数を報告する。W09。
    let accepted = stdout.lines().filter(|l| l.contains("ACCEPTED")).count();
    assert!(
        accepted >= fixtures.len(),
        "each fixture must contribute at least one accepted item (accepted={accepted}, \
         fixtures={})",
        fixtures.len()
    );
    println!(
        "real Codex parser accepted {accepted} items across {} fixtures: {:?}",
        fixtures.len(),
        fixtures
    );
}
