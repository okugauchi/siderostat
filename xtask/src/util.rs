//! Small shared helpers for the xtask runner.

use anyhow::{Context, Result};
use std::{
    env,
    ffi::OsStr,
    io::{self, Read, Seek, SeekFrom, Write},
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
    /// SHA-256 over bounded, evenly distributed file samples. This is a fast
    /// metadata-drift check, not a replacement for the cached full-file digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_sha256: Option<String>,
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
    if let Some(cached) = cache.entries.get(key).cloned()
        && cached.meta == meta
    {
        if cached.sample_sha256.is_none() {
            let sample_sha256 = sampled_sha256(path, meta.size)?;
            ensure_metadata_unchanged(path, meta, label, "sampling")?;
            cache.entries.insert(
                key.to_string(),
                CachedDigest {
                    sample_sha256: Some(sample_sha256),
                    ..cached.clone()
                },
            );
        }
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
    let sample_sha256 = sampled_sha256(path, meta.size)?;
    ensure_metadata_unchanged(path, meta, label, "fingerprinting")?;
    tracing_log(&format!("{label} hash complete -> {digest}"));
    cache.entries.insert(
        key.to_string(),
        CachedDigest {
            meta,
            digest: digest.clone(),
            sample_sha256: Some(sample_sha256),
        },
    );
    Ok((digest, false))
}

/// Reuse a cached full-file digest without re-reading the whole file.
///
/// Exact metadata matches are reused immediately. Metadata drift with an
/// unchanged size is checked using a bounded sampled signature. A legacy cache
/// without that signature requires explicit operator acceptance. Size changes
/// and sampled-content mismatches always require a full fingerprint.
pub fn sha256_from_cache(
    path: &Path,
    label: &str,
    key: &str,
    cache: &mut DigestCache,
    accept_metadata_change: bool,
) -> Result<String> {
    let meta = file_meta(path)?;
    let cached = cache.entries.get(key).cloned().with_context(|| {
        format!(
            "{label} SHA-256 is not cached for {}; run `cargo xtask fingerprint-models` first",
            path.display()
        )
    })?;

    if cached.meta == meta {
        if cached.sample_sha256.is_none() {
            let sample_sha256 = sampled_sha256(path, meta.size)?;
            ensure_metadata_unchanged(path, meta, label, "sampling")?;
            cache.entries.insert(
                key.to_string(),
                CachedDigest {
                    sample_sha256: Some(sample_sha256),
                    ..cached.clone()
                },
            );
        }
        tracing_log(&format!(
            "{label} unchanged ({size_mib} MiB) -> using cached digest",
            size_mib = meta.size / (1024 * 1024)
        ));
        return Ok(cached.digest);
    }

    if cached.meta.size != meta.size {
        anyhow::bail!(
            "{label} size differs from its cached record at {} (cached {} bytes, current {} bytes); run `cargo xtask fingerprint-models` to compute a new full SHA-256",
            path.display(),
            cached.meta.size,
            meta.size
        );
    }

    let sample_sha256 = sampled_sha256(path, meta.size)?;
    ensure_metadata_unchanged(path, meta, label, "sampling")?;
    match cached.sample_sha256.as_deref() {
        Some(expected) if expected != sample_sha256 => {
            anyhow::bail!(
                "{label} metadata changed and its quick content check differs at {}; run `cargo xtask fingerprint-models` to compute a new full SHA-256",
                path.display()
            );
        }
        None if !accept_metadata_change => {
            anyhow::bail!(
                "{label} metadata changed at {}, but this legacy cache cannot determine whether the content changed. If the file was only moved, copied, or touched, rerun install with `--accept-model-metadata-change`; otherwise run `cargo xtask fingerprint-models`",
                path.display()
            );
        }
        Some(_) => tracing_log(&format!(
            "{label} metadata changed, quick content check matched -> refreshing cached metadata"
        )),
        None => tracing_log(&format!(
            "{label} metadata change accepted by operator -> refreshing legacy cache without a full rehash"
        )),
    }

    cache.entries.insert(
        key.to_string(),
        CachedDigest {
            meta,
            digest: cached.digest.clone(),
            sample_sha256: Some(sample_sha256),
        },
    );
    Ok(cached.digest)
}

const SAMPLE_CHUNK_BYTES: u64 = 64 * 1024;
const SAMPLE_COUNT: u64 = 64;

/// Hash at most 4 MiB distributed across the file. Small files are read in
/// full. The file size and each sampled offset are part of the signature.
fn sampled_sha256(path: &Path, size: u64) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(b"siderostat-sampled-sha256-v1\0");
    hasher.update(size.to_le_bytes());

    if size <= SAMPLE_CHUNK_BYTES * SAMPLE_COUNT {
        io::copy(&mut file, &mut DigestWriter(&mut hasher))?;
        return Ok(hex(&hasher.finalize()));
    }

    let max_offset = size - SAMPLE_CHUNK_BYTES;
    let mut buffer = vec![0_u8; SAMPLE_CHUNK_BYTES as usize];
    for index in 0..SAMPLE_COUNT {
        let offset = u64::try_from(
            (u128::from(index) * u128::from(max_offset)) / u128::from(SAMPLE_COUNT - 1),
        )
        .context("sample offset exceeds u64")?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut buffer)?;
        hasher.update(offset.to_le_bytes());
        hasher.update(&buffer);
    }
    Ok(hex(&hasher.finalize()))
}

struct DigestWriter<'a, D>(&'a mut D);

impl<D: sha2::Digest> Write for DigestWriter<'_, D> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn ensure_metadata_unchanged(
    path: &Path,
    expected: FileMeta,
    label: &str,
    operation: &str,
) -> Result<()> {
    let current = file_meta(path)?;
    anyhow::ensure!(
        current == expected,
        "{label} metadata changed while {operation} at {}; retry after the file is stable",
        path.display()
    );
    Ok(())
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

    fn temporary_directory(label: &str) -> Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "siderostat-xtask-util-{label}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn replace_file(path: &Path, content: &[u8]) -> Result<()> {
        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, content)?;
        std::fs::rename(replacement, path)?;
        Ok(())
    }

    #[test]
    fn cached_digest_reuses_matching_file_without_rehashing() -> Result<()> {
        let dir = temporary_directory("reuse")?;
        let path = dir.join("model.gguf");
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"model")?;
            let mut cache = DigestCache::default();
            let (expected, reused) = sha256_cached(&path, "model", "model", &mut cache)?;
            assert!(!reused);
            assert_eq!(
                sha256_from_cache(&path, "model", "model", &mut cache, false)?,
                expected
            );
            assert!(cache.entries["model"].sample_sha256.is_some());
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn cached_digest_requires_an_entry() -> Result<()> {
        let dir = temporary_directory("missing")?;
        let path = dir.join("model.gguf");
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"model")?;
            let mut cache = DigestCache::default();
            let error = sha256_from_cache(&path, "model", "model", &mut cache, false)
                .expect_err("missing cache entry must fail");
            assert!(error.to_string().contains("fingerprint-models"));
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn legacy_cache_json_without_sample_signature_remains_readable() -> Result<()> {
        let cache: DigestCache = serde_json::from_str(
            r#"{
                "entries": {
                    "model": {
                        "meta": {
                            "device_id": 1,
                            "inode": 2,
                            "size": 5,
                            "modified_nanos": 3
                        },
                        "digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    }
                }
            }"#,
        )?;
        assert_eq!(cache.entries["model"].sample_sha256, None);
        Ok(())
    }

    #[test]
    fn exact_legacy_cache_match_backfills_sample_without_full_rehash() -> Result<()> {
        let dir = temporary_directory("legacy-backfill")?;
        let path = dir.join("model.gguf");
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"model")?;
            let expected = sha256_hex(&path)?;
            let mut cache = DigestCache::default();
            cache.entries.insert(
                "model".into(),
                CachedDigest {
                    meta: file_meta(&path)?,
                    digest: expected.clone(),
                    sample_sha256: None,
                },
            );

            assert_eq!(
                sha256_from_cache(&path, "model", "model", &mut cache, false)?,
                expected
            );
            assert!(cache.entries["model"].sample_sha256.is_some());
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn sampled_signature_refreshes_same_content_metadata_drift() -> Result<()> {
        let dir = temporary_directory("sample-refresh")?;
        let path = dir.join("model.gguf");
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"model")?;
            let mut cache = DigestCache::default();
            let (expected, _) = sha256_cached(&path, "model", "model", &mut cache)?;
            let original_meta = cache.entries["model"].meta;

            replace_file(&path, b"model")?;
            assert_ne!(file_meta(&path)?, original_meta);
            assert_eq!(
                sha256_from_cache(&path, "model", "model", &mut cache, false)?,
                expected
            );
            assert_eq!(cache.entries["model"].meta, file_meta(&path)?);
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn sampled_signature_rejects_same_size_content_change() -> Result<()> {
        let dir = temporary_directory("sample-change")?;
        let path = dir.join("model.gguf");
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"model")?;
            let mut cache = DigestCache::default();
            let _ = sha256_cached(&path, "model", "model", &mut cache)?;

            replace_file(&path, b"other")?;
            let error = sha256_from_cache(&path, "model", "model", &mut cache, false)
                .expect_err("sampled content change must fail");
            assert!(error.to_string().contains("quick content check differs"));
            let accepted = sha256_from_cache(&path, "model", "model", &mut cache, true)
                .expect_err("operator acceptance must not override a sample mismatch");
            assert!(accepted.to_string().contains("quick content check differs"));
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn operator_can_migrate_legacy_cache_after_same_size_metadata_drift() -> Result<()> {
        let dir = temporary_directory("legacy")?;
        let path = dir.join("model.gguf");
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"model")?;
            let mut cache = DigestCache::default();
            let (expected, _) = sha256_cached(&path, "model", "model", &mut cache)?;
            cache.entries.get_mut("model").unwrap().sample_sha256 = None;
            replace_file(&path, b"model")?;

            let error = sha256_from_cache(&path, "model", "model", &mut cache, false)
                .expect_err("legacy metadata drift requires operator acceptance");
            assert!(error.to_string().contains("--accept-model-metadata-change"));
            assert_eq!(
                sha256_from_cache(&path, "model", "model", &mut cache, true)?,
                expected
            );
            assert!(cache.entries["model"].sample_sha256.is_some());
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn size_change_always_requires_a_full_fingerprint() -> Result<()> {
        let dir = temporary_directory("size-change")?;
        let path = dir.join("model.gguf");
        let result = (|| -> Result<()> {
            std::fs::write(&path, b"model")?;
            let mut cache = DigestCache::default();
            let _ = sha256_cached(&path, "model", "model", &mut cache)?;
            replace_file(&path, b"larger")?;

            let error = sha256_from_cache(&path, "model", "model", &mut cache, true)
                .expect_err("operator acceptance must not permit a size change");
            assert!(error.to_string().contains("size differs"));
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }
}
