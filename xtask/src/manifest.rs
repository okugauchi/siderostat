//! Standalone / distributed manifest generation.
//!
//! Reuses the application's own manifest types and argv builders so the
//! generated manifests are exactly the ones the app validates at startup
//! (`validate_dspark_binding`, deployment compatibility).

use crate::util;
use anyhow::{Context, Result};
use siderostat::cluster::{
    DEPLOYMENT_MANIFEST_SCHEMA_VERSION, DistributedManifest, StandaloneManifest, argv_sha256,
    build_distributed_worker_command, build_standalone_command,
};
use siderostat::config::ModeAwareConfig;
use std::path::Path;

pub const DIGEST_CACHE_FILE_NAME: &str = "digest-cache.json";

/// Compute the digest inputs from the (already expanded) config and write both
/// manifests to the paths referenced by the config.
pub fn generate(
    config: &ModeAwareConfig,
    ds4_source_commit: Option<&str>,
    approved: &[String],
) -> Result<()> {
    // Digest cache lives beside the MXFP4 manifest so re-installs skip re-reading
    // the (multi-GB) model files when their metadata is unchanged.
    let cache_path = config
        .ds4
        .mxfp4
        .model_manifest
        .parent()
        .context("mxfp4 model_manifest has no parent directory")?
        .join(DIGEST_CACHE_FILE_NAME);
    let mut digest_cache = util::load_digest_cache(&cache_path);

    let ds4_digest = util::sha256_cached(
        &config.ds4.binary,
        "ds4 binary",
        "ds4-binary",
        &mut digest_cache,
    )?
    .0;

    // Standalone manifest.
    let standalone_model_digest = model_digest(
        &config.ds4.standalone.model,
        "standalone model",
        "standalone-model",
        &mut digest_cache,
    )?;
    let standalone_command = build_standalone_command(&config.ds4)
        .map_err(|error| anyhow::anyhow!("build standalone command: {error}"))?;
    let standalone_argv_profile = util::hex(&argv_sha256(
        standalone_command.executable.as_os_str(),
        &standalone_command.argv,
    ));

    let dspark_enabled = config.ds4.dspark.enabled;
    let (dspark_digest, dspark_size) = if dspark_enabled {
        let support = config
            .ds4
            .dspark
            .support_model
            .as_ref()
            .context("DSpark enabled but no support model in config")?;
        (
            model_digest(
                support,
                "dspark support model",
                "dspark-support",
                &mut digest_cache,
            )?,
            util::file_size(support)?,
        )
    } else {
        (String::new(), 0)
    };

    let standalone = StandaloneManifest {
        schema_version: DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
        profile: "standalone".into(),
        profile_id: config.ds4.standalone.profile_id.clone(),
        ds4_binary_sha256: ds4_digest.clone(),
        model_sha256: standalone_model_digest,
        checkpoint: config.ds4.standalone.checkpoint.clone(),
        model_variant: config.ds4.standalone.model_variant.name().to_string(),
        residency: config.ds4.standalone.residency.name().to_string(),
        context_size: config.ds4.standalone.context_size as u64,
        argv_profile_sha256: standalone_argv_profile,
        dspark_enabled,
        dspark_support_sha256: if dspark_enabled {
            Some(dspark_digest)
        } else {
            None
        },
        dspark_support_size: if dspark_enabled {
            Some(dspark_size)
        } else {
            None
        },
        dspark_confidence: config.ds4.dspark.confidence,
        dspark_strict: config.ds4.dspark.strict,
    };
    standalone
        .validate()
        .context("standalone manifest validation failed")?;
    let standalone_json = serde_json::to_string_pretty(&standalone)?;
    util::write(
        &config.ds4.standalone.model_manifest,
        standalone_json.as_bytes(),
    )
    .context("write standalone manifest")?;
    util::tracing_log(&format!(
        "wrote standalone manifest -> {}",
        config.ds4.standalone.model_manifest.display()
    ));

    // Distributed manifest (MXFP4). The worker command argv is used for the
    // recorded argv profile; both nodes must generate from the same ds4.mxfp4
    // config so the values agree (spec: MXFP4 config is shared).
    let mxfp4_digest = model_digest(
        &config.ds4.mxfp4.model,
        "mxfp4 model",
        "mxfp4-model",
        &mut digest_cache,
    )?;
    let mxfp4_size = util::file_size(&config.ds4.mxfp4.model)?;
    let worker_command = build_distributed_worker_command(
        &config.ds4,
        config.cluster.coordinator_address,
        config.cluster.ds4_distributed_port,
    )
    .map_err(|error| anyhow::anyhow!("build distributed worker command: {error}"))?;
    let distributed_argv_profile = util::hex(&argv_sha256(
        worker_command.executable.as_os_str(),
        &worker_command.argv,
    ));

    let compatible = resolve_compatible_digests(&ds4_digest, approved)?;
    let source_commit = resolve_source_commit(ds4_source_commit)?;

    let distributed = DistributedManifest {
        schema_version: DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
        profile: "distributed-mxfp4".into(),
        ds4_binary_sha256: ds4_digest,
        compatible_ds4_binary_sha256: compatible,
        ds4_source_commit: source_commit,
        model_sha256: mxfp4_digest,
        model_size: mxfp4_size,
        checkpoint: config.ds4.mxfp4.checkpoint.clone(),
        model_family: "DeepSeek V4 Flash".into(),
        quantization: "mxfp4-experts".into(),
        context_size: config.ds4.mxfp4.context_size as u64,
        coordinator_layers: config.ds4.mxfp4.coordinator_layers.clone(),
        worker_layers: config.ds4.mxfp4.worker_layers.clone(),
        ds4_wire_schema: "ds4d-v1-hello40".into(),
        argv_profile_sha256: distributed_argv_profile,
    };
    distributed
        .validate()
        .context("distributed manifest validation failed")?;
    let distributed_json = serde_json::to_string_pretty(&distributed)?;
    util::write(
        &config.ds4.mxfp4.model_manifest,
        distributed_json.as_bytes(),
    )
    .context("write distributed manifest")?;
    util::tracing_log(&format!(
        "wrote distributed manifest -> {}",
        config.ds4.mxfp4.model_manifest.display()
    ));

    util::save_digest_cache(&cache_path, &digest_cache)?;

    Ok(())
}

/// Compute and persist the model digests used by install without generating
/// manifests or changing the installed service.
pub fn fingerprint_models(
    cache_path: &Path,
    standalone: &Path,
    mxfp4: &Path,
    dspark_support: Option<&Path>,
) -> Result<()> {
    let mut digest_cache = util::load_digest_cache(cache_path);
    for (path, label, key) in [
        (standalone, "standalone model", "standalone-model"),
        (mxfp4, "mxfp4 model", "mxfp4-model"),
    ] {
        let (digest, _) = util::sha256_cached(path, label, key, &mut digest_cache)?;
        util::tracing_log(&format!(
            "{label}: sha256={digest}, size={} bytes",
            util::file_size(path)?
        ));
    }
    if let Some(path) = dspark_support {
        let (digest, _) = util::sha256_cached(
            path,
            "dspark support model",
            "dspark-support",
            &mut digest_cache,
        )?;
        util::tracing_log(&format!(
            "dspark support model: sha256={digest}, size={} bytes",
            util::file_size(path)?
        ));
    }
    util::save_digest_cache(cache_path, &digest_cache)?;
    util::tracing_log(&format!(
        "model SHA-256 cache saved -> {}",
        cache_path.display()
    ));
    Ok(())
}

/// Check that install can reuse the model digests without reading GGUF
/// contents. This is intentionally a separate preflight so a failed install
/// does not build or replace binaries when the cache is missing or stale.
pub fn verify_model_cache(
    cache_path: &Path,
    standalone: &Path,
    mxfp4: &Path,
    dspark_support: Option<&Path>,
    accept_metadata_change: bool,
) -> Result<()> {
    let mut digest_cache = util::load_digest_cache(cache_path);
    let original_cache = digest_cache.clone();
    for (path, label, key) in [
        (standalone, "standalone model", "standalone-model"),
        (mxfp4, "mxfp4 model", "mxfp4-model"),
    ] {
        let _ =
            util::sha256_from_cache(path, label, key, &mut digest_cache, accept_metadata_change)?;
    }
    if let Some(path) = dspark_support {
        let _ = util::sha256_from_cache(
            path,
            "dspark support model",
            "dspark-support",
            &mut digest_cache,
            accept_metadata_change,
        )?;
    }
    if digest_cache != original_cache {
        util::save_digest_cache(cache_path, &digest_cache)?;
        util::tracing_log(&format!(
            "model SHA-256 cache metadata refreshed -> {}",
            cache_path.display()
        ));
    }
    Ok(())
}

fn model_digest(
    path: &Path,
    label: &str,
    key: &str,
    cache: &mut util::DigestCache,
) -> Result<String> {
    util::sha256_from_cache(path, label, key, cache, false)
}

/// The approved ds4 binary digest set. If `approved` is provided, it must include
/// the locally installed digest (fail closed otherwise). If empty, the locally
/// installed digest is the sole approved entry.
fn resolve_compatible_digests(local: &str, approved: &[String]) -> Result<Vec<String>> {
    let mut set: Vec<String> = if approved.is_empty() {
        vec![local.to_string()]
    } else {
        approved.to_vec()
    };
    set.sort();
    set.dedup();
    if !set.iter().any(|digest| digest == local) {
        anyhow::bail!(
            "local ds4 binary digest {local} is not in the approved compatible set; \
             pass --ds4-binary-digest with the approved digest(s)"
        );
    }
    Ok(set)
}

/// The verified DS4 source commit for the distributed manifest. It must come from
/// the operator (--ds4-source-commit); it cannot be derived from local files.
fn resolve_source_commit(explicit: Option<&str>) -> Result<String> {
    explicit
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .context("distributed manifest requires --ds4-source-commit (verified DS4 checkout commit)")
}

/// Read an existing distributed manifest if present and return its operator-only
/// fields so a re-install can preserve them. Returns None if absent/unreadable.
pub fn read_existing_distributed(path: &Path) -> Result<Option<DistributedManifest>> {
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    match serde_json::from_slice::<DistributedManifest>(&bytes) {
        Ok(manifest) => Ok(Some(manifest)),
        Err(error) => {
            util::tracing_log(&format!(
                "existing distributed manifest {} is not parseable: {error}",
                path.display()
            ));
            Ok(None)
        }
    }
}
