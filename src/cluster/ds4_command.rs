use crate::config::{Ds4Config, Quantization, Residency, SpeculativeSupport, validate_extra_args};
use std::{ffi::OsString, net::IpAddr, path::PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ds4Profile {
    pub profile_id: String,
    pub quantization: Quantization,
    pub residency: Residency,
    pub speculative_support: SpeculativeSupport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ds4Command {
    pub executable: PathBuf,
    pub working_directory: PathBuf,
    pub argv: Vec<OsString>,
    pub profile: Ds4Profile,
}

impl Ds4Command {
    pub fn tokio_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.executable);
        command.current_dir(&self.working_directory);
        command.args(&self.argv);
        command
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Ds4CommandError {
    #[error("standalone SSD options require residency ssd-streaming")]
    SsdOptionsForResident,
    #[error("invalid DS4 extra arguments: {0}")]
    InvalidExtraArguments(String),
    #[error("distributed DS4 profiles require --debug")]
    DistributedDebugRequired,
    #[error("DSpark requires a support model")]
    DsparkSupportModelRequired,
    #[error("DSpark is not compatible with standalone SSD streaming")]
    DsparkSsdStreaming,
}

pub fn build_distributed_worker_command(
    config: &Ds4Config,
    coordinator_address: IpAddr,
    distributed_port: u16,
) -> Result<Ds4Command, Ds4CommandError> {
    let distributed = &config.distributed;
    validate_distributed_extra_args(distributed)?;

    let mut argv = vec![
        OsString::from("-m"),
        distributed.model.as_os_str().to_owned(),
        OsString::from("--role"),
        OsString::from("worker"),
        OsString::from("--layers"),
        OsString::from(&distributed.worker_layers),
        OsString::from("--coordinator"),
        OsString::from(coordinator_address.to_string()),
        OsString::from(distributed_port.to_string()),
        OsString::from("--ctx"),
        OsString::from(distributed.context_size.to_string()),
        OsString::from("--kv-disk-dir"),
        distributed.kv_disk_dir.as_os_str().to_owned(),
        OsString::from("--kv-disk-space-mb"),
        OsString::from(distributed.kv_disk_space_mb.to_string()),
    ];
    argv.extend(distributed.extra_args.iter().map(OsString::from));

    Ok(distributed_command(config, "worker", argv))
}

pub fn build_distributed_coordinator_command(
    config: &Ds4Config,
    coordinator_address: IpAddr,
    distributed_port: u16,
) -> Result<Ds4Command, Ds4CommandError> {
    let distributed = &config.distributed;
    validate_distributed_extra_args(distributed)?;
    let mut argv = vec![
        OsString::from("-m"),
        distributed.model.as_os_str().to_owned(),
        OsString::from("--role"),
        OsString::from("coordinator"),
        OsString::from("--layers"),
        OsString::from(&distributed.coordinator_layers),
        OsString::from("--listen"),
        OsString::from(coordinator_address.to_string()),
        OsString::from(distributed_port.to_string()),
        OsString::from("--host"),
        OsString::from(config.http_host.to_string()),
        OsString::from("--port"),
        OsString::from(config.http_port.to_string()),
        OsString::from("--ctx"),
        OsString::from(distributed.context_size.to_string()),
        OsString::from("--kv-disk-dir"),
        distributed.kv_disk_dir.as_os_str().to_owned(),
        OsString::from("--kv-disk-space-mb"),
        OsString::from(distributed.kv_disk_space_mb.to_string()),
    ];
    argv.extend(distributed.extra_args.iter().map(OsString::from));
    Ok(distributed_command(config, "coordinator", argv))
}

fn validate_distributed_extra_args(
    distributed: &crate::config::Ds4DistributedConfig,
) -> Result<(), Ds4CommandError> {
    validate_extra_args("ds4.distributed.extra_args", &distributed.extra_args)
        .map_err(|error| Ds4CommandError::InvalidExtraArguments(error.to_string()))?;
    if !distributed
        .extra_args
        .iter()
        .any(|argument| argument == "--debug")
    {
        return Err(Ds4CommandError::DistributedDebugRequired);
    }
    Ok(())
}

fn distributed_command(config: &Ds4Config, role: &str, argv: Vec<OsString>) -> Ds4Command {
    Ds4Command {
        executable: config.binary.clone(),
        working_directory: config.working_directory.clone(),
        argv,
        profile: Ds4Profile {
            profile_id: format!("distributed-layer-parallel-{role}"),
            quantization: config.distributed.quantization,
            residency: Residency::Resident,
            speculative_support: SpeculativeSupport::None,
        },
    }
}

pub fn build_standalone_command(config: &Ds4Config) -> Result<Ds4Command, Ds4CommandError> {
    let standalone = &config.standalone;
    validate_extra_args("ds4.standalone.extra_args", &standalone.extra_args)
        .map_err(|error| Ds4CommandError::InvalidExtraArguments(error.to_string()))?;
    let has_ssd_options = standalone.ssd_cache_experts.is_some()
        || standalone.ssd_full_layers.is_some()
        || standalone.ssd_preload_experts.is_some()
        || standalone.ssd_cold;
    if standalone.residency == Residency::Resident && has_ssd_options {
        return Err(Ds4CommandError::SsdOptionsForResident);
    }
    if config.dspark.enabled && standalone.residency == Residency::SsdStreaming {
        return Err(Ds4CommandError::DsparkSsdStreaming);
    }

    let mut argv = vec![
        OsString::from("-m"),
        standalone.model.as_os_str().to_owned(),
        OsString::from("--host"),
        OsString::from(config.http_host.to_string()),
        OsString::from("--port"),
        OsString::from(config.http_port.to_string()),
        OsString::from("--ctx"),
        OsString::from(standalone.context_size.to_string()),
        OsString::from("--kv-disk-dir"),
        standalone.kv_disk_dir.as_os_str().to_owned(),
        OsString::from("--kv-disk-space-mb"),
        OsString::from(standalone.kv_disk_space_mb.to_string()),
    ];
    if standalone.residency == Residency::SsdStreaming {
        argv.push(OsString::from("--ssd-streaming"));
        push_option(
            &mut argv,
            "--ssd-streaming-cache-experts",
            standalone.ssd_cache_experts.as_deref(),
        );
        push_option(
            &mut argv,
            "--ssd-streaming-full-layers",
            standalone
                .ssd_full_layers
                .map(|value| value.to_string())
                .as_deref(),
        );
        // Upstream treats zero as disabled; omit it instead of emitting an argument it rejects.
        if let Some(value) = standalone.ssd_preload_experts.filter(|value| *value > 0) {
            argv.push(OsString::from("--ssd-streaming-preload-experts"));
            argv.push(OsString::from(value.to_string()));
        }
        if standalone.ssd_cold {
            argv.push(OsString::from("--ssd-streaming-cold"));
        }
    }
    if config.dspark.enabled {
        let support_model = config
            .dspark
            .support_model
            .as_ref()
            .ok_or(Ds4CommandError::DsparkSupportModelRequired)?;
        argv.push(OsString::from("--mtp"));
        argv.push(support_model.as_os_str().to_owned());
        argv.push(OsString::from("--dspark"));
        if let Some(confidence) = config.dspark.confidence {
            argv.push(OsString::from("--dspark-confidence"));
            argv.push(OsString::from(confidence.to_string()));
        }
        if config.dspark.strict {
            argv.push(OsString::from("--dspark-strict"));
        }
    }
    argv.extend(standalone.extra_args.iter().map(OsString::from));

    Ok(Ds4Command {
        executable: config.binary.clone(),
        working_directory: config.working_directory.clone(),
        argv,
        profile: Ds4Profile {
            profile_id: standalone.profile_id.clone(),
            quantization: standalone.quantization,
            residency: standalone.residency,
            speculative_support: if config.dspark.enabled {
                SpeculativeSupport::Dspark
            } else {
                SpeculativeSupport::None
            },
        },
    })
}

fn push_option(argv: &mut Vec<OsString>, name: &str, value: Option<&str>) {
    if let Some(value) = value {
        argv.push(OsString::from(name));
        argv.push(OsString::from(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Ds4DistributedConfig, Ds4DsparkConfig, Ds4StandaloneConfig};
    use std::{net::IpAddr, path::PathBuf};

    fn config(variant: Quantization, residency: Residency) -> Ds4Config {
        Ds4Config {
            binary: PathBuf::from("/opt/ds4/bin/ds4-server"),
            working_directory: PathBuf::from("/opt/ds4 working"),
            http_host: IpAddr::from([127, 0, 0, 1]),
            http_port: 8000,
            allow_sigkill: false,
            dspark: Ds4DsparkConfig::default(),
            standalone: Ds4StandaloneConfig {
                profile_id: format!("{variant:?}-{residency:?}"),
                model: PathBuf::from("/models/DeepSeek V4.gguf"),
                model_manifest: PathBuf::from("/manifests/standalone.json"),
                checkpoint: "flash-0731".into(),
                quantization: variant,
                residency,
                context_size: 262_144,
                kv_disk_dir: PathBuf::from("/cache/standalone profile"),
                kv_disk_space_mb: 262_144,
                ssd_cache_experts: None,
                ssd_full_layers: None,
                ssd_preload_experts: None,
                ssd_cold: false,
                extra_args: vec!["--quality".into()],
            },
            distributed: Ds4DistributedConfig {
                topology: crate::config::DistributedTopology::LayerParallel,
                quantization: Quantization::Mxfp4,
                model: PathBuf::from("/models/distributed.gguf"),
                model_manifest: PathBuf::from("/manifests/distributed.json"),
                checkpoint: "flash-0731".into(),
                context_size: 262_144,
                coordinator_layers: "0:19".into(),
                worker_layers: "20:output".into(),
                kv_disk_dir: PathBuf::from("/cache/distributed"),
                kv_disk_space_mb: 262_144,
                extra_args: vec!["--debug".into()],
            },
        }
    }

    fn argv(command: &Ds4Command) -> Vec<String> {
        command
            .argv
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn standalone_variant_and_residency_matrix_generates_one_complete_argv() {
        for variant in [Quantization::Q2, Quantization::Q2Q4, Quantization::Mxfp4] {
            for residency in [Residency::Resident, Residency::SsdStreaming] {
                let command = build_standalone_command(&config(variant, residency)).unwrap();
                let values = argv(&command);
                assert_eq!(command.profile.quantization, variant);
                assert_eq!(command.profile.residency, residency);
                assert_eq!(
                    values[0..4],
                    ["-m", "/models/DeepSeek V4.gguf", "--host", "127.0.0.1"]
                );
                assert!(values.windows(2).any(|pair| pair == ["--ctx", "262144"]));
                assert!(
                    values
                        .windows(2)
                        .any(|pair| pair == ["--kv-disk-dir", "/cache/standalone profile"])
                );
                assert_eq!(
                    values
                        .iter()
                        .filter(|value| *value == "--ssd-streaming")
                        .count(),
                    usize::from(residency == Residency::SsdStreaming)
                );
                assert_eq!(values.last().unwrap(), "--quality");
            }
        }
    }

    #[test]
    fn typed_ssd_options_are_emitted_once_and_zero_preload_is_omitted() {
        let mut config = config(Quantization::Mxfp4, Residency::SsdStreaming);
        config.standalone.ssd_cache_experts = Some("32GB".into());
        config.standalone.ssd_full_layers = Some(0);
        config.standalone.ssd_preload_experts = Some(0);
        config.standalone.ssd_cold = true;
        let values = argv(&build_standalone_command(&config).unwrap());
        assert!(
            values
                .windows(2)
                .any(|pair| pair == ["--ssd-streaming-cache-experts", "32GB"])
        );
        assert!(
            values
                .windows(2)
                .any(|pair| pair == ["--ssd-streaming-full-layers", "0"])
        );
        assert!(
            !values
                .iter()
                .any(|value| value == "--ssd-streaming-preload-experts")
        );
        assert_eq!(
            values
                .iter()
                .filter(|value| *value == "--ssd-streaming-cold")
                .count(),
            1
        );
    }

    #[test]
    fn builder_rejects_generated_option_override_and_resident_ssd_tuning() {
        let mut duplicate = config(Quantization::Q2, Residency::SsdStreaming);
        duplicate.standalone.extra_args.push("--ctx=1".into());
        assert!(matches!(
            build_standalone_command(&duplicate),
            Err(Ds4CommandError::InvalidExtraArguments(_))
        ));

        let mut resident = config(Quantization::Q2Q4, Residency::Resident);
        resident.standalone.ssd_cache_experts = Some("8".into());
        assert_eq!(
            build_standalone_command(&resident),
            Err(Ds4CommandError::SsdOptionsForResident)
        );
    }

    #[test]
    fn standalone_dspark_argv_is_typed_and_emitted_once() {
        let mut config = config(Quantization::Q2Q4, Residency::Resident);
        config.dspark = Ds4DsparkConfig {
            enabled: true,
            support_model: Some(PathBuf::from("/models/DSpark support.gguf")),
            confidence: Some(0.7),
            strict: true,
        };
        let command = build_standalone_command(&config).unwrap();
        let values = argv(&command);
        assert_eq!(
            command.profile.speculative_support,
            SpeculativeSupport::Dspark
        );
        assert_eq!(values.iter().filter(|value| *value == "--mtp").count(), 1);
        assert_eq!(
            values.iter().filter(|value| *value == "--dspark").count(),
            1
        );
        assert!(
            values
                .windows(2)
                .any(|pair| pair == ["--mtp", "/models/DSpark support.gguf"])
        );
        assert!(
            values
                .windows(2)
                .any(|pair| pair == ["--dspark-confidence", "0.7"])
        );
        assert_eq!(
            values
                .iter()
                .filter(|value| *value == "--dspark-strict")
                .count(),
            1
        );
    }

    #[test]
    fn standalone_dspark_rejects_missing_support_and_ssd_streaming() {
        let mut missing = config(Quantization::Q2, Residency::Resident);
        missing.dspark.enabled = true;
        assert_eq!(
            build_standalone_command(&missing),
            Err(Ds4CommandError::DsparkSupportModelRequired)
        );

        let mut streaming = config(Quantization::Q2, Residency::SsdStreaming);
        streaming.dspark.enabled = true;
        streaming.dspark.support_model = Some(PathBuf::from("/models/support.gguf"));
        assert_eq!(
            build_standalone_command(&streaming),
            Err(Ds4CommandError::DsparkSsdStreaming)
        );
    }

    #[test]
    fn distributed_worker_argv_is_complete_and_has_no_http_listener() {
        let command = build_distributed_worker_command(
            &config(Quantization::Q2, Residency::SsdStreaming),
            IpAddr::from([10, 99, 0, 1]),
            9911,
        )
        .unwrap();
        assert_eq!(
            argv(&command),
            [
                "-m",
                "/models/distributed.gguf",
                "--role",
                "worker",
                "--layers",
                "20:output",
                "--coordinator",
                "10.99.0.1",
                "9911",
                "--ctx",
                "262144",
                "--kv-disk-dir",
                "/cache/distributed",
                "--kv-disk-space-mb",
                "262144",
                "--debug",
            ]
        );
        assert_eq!(
            command.profile.profile_id,
            "distributed-layer-parallel-worker"
        );
        assert_eq!(command.profile.quantization, Quantization::Mxfp4);
        assert!(!command.argv.iter().any(|value| value == "--host"));
        assert!(!command.argv.iter().any(|value| value == "--port"));
    }

    #[test]
    fn distributed_worker_requires_debug_and_rejects_generated_overrides() {
        let mut missing_debug = config(Quantization::Mxfp4, Residency::Resident);
        missing_debug.distributed.extra_args.clear();
        assert_eq!(
            build_distributed_worker_command(&missing_debug, IpAddr::from([10, 99, 0, 1]), 9911,),
            Err(Ds4CommandError::DistributedDebugRequired)
        );

        let mut override_role = config(Quantization::Mxfp4, Residency::Resident);
        override_role
            .distributed
            .extra_args
            .push("--role=coordinator".into());
        assert!(matches!(
            build_distributed_worker_command(&override_role, IpAddr::from([10, 99, 0, 1]), 9911,),
            Err(Ds4CommandError::InvalidExtraArguments(_))
        ));
    }

    #[test]
    fn distributed_coordinator_argv_owns_http_and_rendezvous_listeners() {
        let command = build_distributed_coordinator_command(
            &config(Quantization::Q2Q4, Residency::SsdStreaming),
            IpAddr::from([10, 99, 0, 1]),
            9911,
        )
        .unwrap();
        assert_eq!(
            argv(&command),
            [
                "-m",
                "/models/distributed.gguf",
                "--role",
                "coordinator",
                "--layers",
                "0:19",
                "--listen",
                "10.99.0.1",
                "9911",
                "--host",
                "127.0.0.1",
                "--port",
                "8000",
                "--ctx",
                "262144",
                "--kv-disk-dir",
                "/cache/distributed",
                "--kv-disk-space-mb",
                "262144",
                "--debug",
            ]
        );
        assert_eq!(
            command.profile.profile_id,
            "distributed-layer-parallel-coordinator"
        );
    }
}
