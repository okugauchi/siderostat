//! Small shared helpers for the xtask runner.

use anyhow::{Context, Result};
use std::{
    env,
    ffi::OsStr,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

/// Run a command and require success. Returns stdout bytes on success.
pub fn run(cmd: &str, args: &[&OsStr]) -> Result<Vec<u8>> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("spawn {cmd}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{cmd} failed (status {:?}): {stderr}", out.status.code());
    }
    Ok(out.stdout)
}

/// Run a command that is expected to be interactive (e.g. sudo) and echo its output live.
pub fn run_live(cmd: &str, args: &[&OsStr]) -> Result<()> {
    let status = Command::new(cmd).args(args).status()?;
    if !status.success() {
        anyhow::bail!("{cmd} exited with status {:?}", status.code());
    }
    Ok(())
}

/// Current user's home directory (expanded).
pub fn home() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// Current login user name ($USER or whoami fallback).
pub fn current_user() -> Result<String> {
    if let Some(user) = env::var_os("USER") {
        return Ok(user.to_string_lossy().into_owned());
    }
    let out = run("whoami", &[])?;
    Ok(String::from_utf8(out)?.trim().to_string())
}

/// SHA-256 of a file, lower-case hex, computed by streaming.
pub fn sha256_hex(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Byte length of a file.
pub fn file_size(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len())
}

/// Lower-case hex of a 32-byte digest.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Copy `src` to `dst`, creating the parent directory. If `dst` already exists it
/// is first moved to a timestamped `.bak-<ts>` sibling.
pub fn backup_and_write(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    if dst.exists() {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let backup = dst.with_extension(format!("bak-{ts}"));
        std::fs::rename(dst, &backup)
            .with_context(|| format!("backup {} -> {}", dst.display(), backup.display()))?;
        tracing_log(&format!(
            "backed up {} -> {}",
            dst.display(),
            backup.display()
        ));
    }
    std::fs::copy(src, dst)
        .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

/// Write bytes to a path, creating parents.
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Print a progress line to stderr.
pub fn tracing_log(msg: &str) {
    eprintln!("[xtask] {msg}");
}
