use crate::config::{Ds4Config, ModelVariant, Residency, validate_extra_args};
use std::{ffi::OsString, path::PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ds4Profile {
    pub profile_id: String,
    pub model_variant: ModelVariant,
    pub residency: Residency,
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
    argv.extend(standalone.extra_args.iter().map(OsString::from));

    Ok(Ds4Command {
        executable: config.binary.clone(),
        working_directory: config.working_directory.clone(),
        argv,
        profile: Ds4Profile {
            profile_id: standalone.profile_id.clone(),
            model_variant: standalone.model_variant,
            residency: standalone.residency,
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
    use crate::config::{Ds4Mxfp4Config, Ds4StandaloneConfig};
    use std::{net::IpAddr, path::PathBuf};

    fn config(variant: ModelVariant, residency: Residency) -> Ds4Config {
        Ds4Config {
            binary: PathBuf::from("/opt/ds4/bin/ds4-server"),
            working_directory: PathBuf::from("/opt/ds4 working"),
            http_host: IpAddr::from([127, 0, 0, 1]),
            http_port: 8000,
            allow_sigkill: false,
            standalone: Ds4StandaloneConfig {
                profile_id: format!("{variant:?}-{residency:?}"),
                model: PathBuf::from("/models/DeepSeek V4.gguf"),
                model_manifest: PathBuf::from("/manifests/standalone.json"),
                checkpoint: "flash-0731".into(),
                model_variant: variant,
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
            mxfp4: Ds4Mxfp4Config {
                model: PathBuf::from("/models/mxfp4.gguf"),
                model_manifest: PathBuf::from("/manifests/mxfp4.json"),
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
        for variant in [ModelVariant::Q2, ModelVariant::Q2Q4, ModelVariant::Mxfp4] {
            for residency in [Residency::Resident, Residency::SsdStreaming] {
                let command = build_standalone_command(&config(variant, residency)).unwrap();
                let values = argv(&command);
                assert_eq!(command.profile.model_variant, variant);
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
        let mut config = config(ModelVariant::Mxfp4, Residency::SsdStreaming);
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
        let mut duplicate = config(ModelVariant::Q2, Residency::SsdStreaming);
        duplicate.standalone.extra_args.push("--ctx=1".into());
        assert!(matches!(
            build_standalone_command(&duplicate),
            Err(Ds4CommandError::InvalidExtraArguments(_))
        ));

        let mut resident = config(ModelVariant::Q2Q4, Residency::Resident);
        resident.standalone.ssd_cache_experts = Some("8".into());
        assert_eq!(
            build_standalone_command(&resident),
            Err(Ds4CommandError::SsdOptionsForResident)
        );
    }
}
