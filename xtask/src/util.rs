//! Small shared helpers for the xtask runner.

use anyhow::{Context, Result};
use std::{
    env,
    ffi::OsStr,
    io::{self, Read, Write},
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

/// Compute a SHA-256 with a progress log line before and after, so long hashing
/// of large model files doesn't look like a hang.
pub fn sha256_hex_logged(path: &Path, label: &str) -> Result<String> {
    let size_mib = file_size(path)? / (1024 * 1024);
    tracing_log(&format!(
        "hashing {label} ({size_mib} MiB) -> {}",
        path.display()
    ));
    let digest = sha256_hex(path)?;
    tracing_log(&format!("{label} hash complete -> {digest}"));
    Ok(digest)
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

/// Filesystem metadata that identifies whether a previously recorded digest can
/// be reused without re-reading the file. Mirrors the application's
/// `FileFingerprint` staleness check (device_id + inode + size + modified_nanos).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileMeta {
    pub device_id: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_nanos: u128,
}

/// A cached digest plus the metadata of the file it was computed from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CachedDigest {
    pub meta: FileMeta,
    pub digest: String,
}

/// On-disk digest cache keyed by a stable profile name, so a re-install can
/// reuse previously computed model digests when file metadata is unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DigestCache {
    pub entries: std::collections::HashMap<String, CachedDigest>,
}

/// Stat a file and record the metadata used for digest-cache staleness checks.
pub fn file_meta(path: &Path) -> Result<FileMeta> {
    let metadata = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let modified_nanos = (metadata.mtime() as i128)
            .saturating_mul(1_000_000_000)
            .saturating_add(metadata.mtime_nsec() as i128)
            .try_into()
            .unwrap_or_default();
        Ok(FileMeta {
            device_id: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified_nanos,
        })
    }
    #[cfg(not(unix))]
    {
        Ok(FileMeta {
            device_id: 0,
            inode: 0,
            size: metadata.len(),
            modified_nanos: 0,
        })
    }
}

/// Load a digest cache file if present and parseable. Returns an empty cache
/// when absent or unreadable, so a fresh install still computes digests.
pub fn load_digest_cache(path: &Path) -> DigestCache {
    if !path.is_file() {
        return DigestCache::default();
    }
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => DigestCache::default(),
    }
}

/// Save a digest cache file, creating parent directories.
pub fn save_digest_cache(path: &Path, cache: &DigestCache) -> Result<()> {
    let json = serde_json::to_string_pretty(cache)?;
    write(path, json.as_bytes())
}

/// Compute a SHA-256 for `path`, reusing `cache[key]` when the current file
/// metadata matches the cached record. Otherwise computes and records it.
/// Returns the digest and whether it was reused (no re-read).
pub fn sha256_cached(
    path: &Path,
    label: &str,
    key: &str,
    cache: &mut DigestCache,
) -> Result<(String, bool)> {
    let meta = file_meta(path)?;
    if let Some(cached) = cache.entries.get(key)
        && cached.meta == meta
    {
        tracing_log(&format!(
            "{label} unchanged ({size_mib} MiB) -> reusing cached digest",
            size_mib = meta.size / (1024 * 1024)
        ));
        return Ok((cached.digest.clone(), true));
    }
    let size_mib = meta.size / (1024 * 1024);
    tracing_log(&format!(
        "hashing {label} ({size_mib} MiB) -> {}",
        path.display()
    ));
    let digest = sha256_hex(path)?;
    tracing_log(&format!("{label} hash complete -> {digest}"));
    cache.entries.insert(
        key.to_string(),
        CachedDigest {
            meta,
            digest: digest.clone(),
        },
    );
    Ok((digest, false))
}

/// Reuse a cached digest without reading the file contents. Fails closed when
/// the cache entry is absent or the file metadata no longer matches it.
pub fn sha256_from_cache(
    path: &Path,
    label: &str,
    key: &str,
    cache: &DigestCache,
) -> Result<String> {
    let meta = file_meta(path)?;
    let cached = cache.entries.get(key).with_context(|| {
        format!(
            "{label} SHA-256 is not cached for {}; run `cargo xtask fingerprint-models` first",
            path.display()
        )
    })?;
    if cached.meta != meta {
        anyhow::bail!(
            "{label} changed after its SHA-256 was cached at {}; run `cargo xtask fingerprint-models` again",
            path.display()
        );
    }
    tracing_log(&format!(
        "{label} unchanged ({size_mib} MiB) -> using cached digest",
        size_mib = meta.size / (1024 * 1024)
    ));
    Ok(cached.digest.clone())
}

/// Ask whether an operation should run. Empty input, EOF, and any response
/// other than `y`/`yes` select the default of no.
pub fn confirm_default_no(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        tracing_log("no response; defaulting to no");
        return Ok(false);
    }
    let confirmed = matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if confirmed {
        tracing_log("model SHA-256 calculation enabled");
    } else {
        tracing_log("model SHA-256 calculation skipped");
    }
    Ok(confirmed)
}

/// If `dst` already holds exactly `content`, leave it untouched and return
/// false (idempotent install). Otherwise move an existing `dst` to a
/// timestamped `.bak-<ts>` sibling and write `content`. Creates parents.
pub fn backup_and_write_if_changed(dst: &Path, content: &[u8]) -> Result<bool> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let same = if dst.exists() {
        match std::fs::read(dst) {
            Ok(bytes) => bytes == content,
            Err(_) => false,
        }
    } else {
        false
    };
    if same {
        tracing_log(&format!("unchanged, keeping {}", dst.display()));
        return Ok(false);
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
    std::fs::write(dst, content).with_context(|| format!("write {}", dst.display()))?;
    Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn cached_digest_reuses_matching_file_without_rehashing() -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "siderostat-xtask-util-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("model.gguf");
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"model")?;
            let mut cache = DigestCache::default();
            let (expected, reused) = sha256_cached(&path, "model", "model", &mut cache)?;
            assert!(!reused);
            assert_eq!(
                sha256_from_cache(&path, "model", "model", &cache)?,
                expected
            );
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn cached_digest_requires_an_entry() -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "siderostat-xtask-util-missing-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("model.gguf");
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"model")?;
            let error = sha256_from_cache(&path, "model", "model", &DigestCache::default())
                .expect_err("missing cache entry must fail");
            assert!(error.to_string().contains("fingerprint-models"));
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }
}
