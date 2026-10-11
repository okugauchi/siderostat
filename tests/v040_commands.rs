//! T04 — TP command builder と引数 golden の受入 case。。。。。
#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    Ds4CommandError, build_distributed_coordinator_command, build_distributed_worker_command,
    build_tp_coordinator_command, build_tp_worker_command,
};
use siderostat::config::{
    DistributedTopology, Ds4Config, Ds4DistributedConfig, Ds4DsparkConfig, Ds4StandaloneConfig,
    Quantization, Residency, Transport,
};
use std::ffi::OsStr;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

fn base_config() -> Ds4Config {
    Ds4Config {
        binary: PathBuf::from("/usr/local/bin/ds4-server"),
        working_directory: PathBuf::from("/work/ds4"),
        http_host: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        http_port: 8000,
        allow_sigkill: true,
        dspark: Ds4DsparkConfig::default(),
        standalone: Ds4StandaloneConfig {
            profile_id: "standalone".into(),
            model: PathBuf::from("/models/standalone.gguf"),
            model_manifest: PathBuf::from("/manifests/standalone.json"),
            checkpoint: "flash-0731".into(),
            quantization: Quantization::Mxfp4,
            residency: Residency::Resident,
            context_size: 262_144,
            kv_disk_dir: PathBuf::from("/cache/standalone"),
            kv_disk_space_mb: 262_144,
            ssd_cache_experts: None,
            ssd_full_layers: None,
            ssd_preload_experts: None,
            ssd_cold: false,
            extra_args: vec![],
        },
        distributed: Ds4DistributedConfig {
            topology: DistributedTopology::TensorParallel,
            quantization: Quantization::Mxfp4,
            transport: Transport::Rdma,
            model: PathBuf::from("/models/mxfp4.gguf"),
            model_manifest: PathBuf::from("/manifests/mxfp4.json"),
            checkpoint: "flash-0731".into(),
            context_size: 8192,
            coordinator_layers: None,
            worker_layers: None,
            kv_disk_dir: PathBuf::from("/cache/distributed"),
            kv_disk_space_mb: 262_144,
            extra_args: vec![],
            role_artifact: None,
            capability_manifest: None,
            rdma_member_interface: None,
            rdma_address: None,
            rdma_device: None,
            rdma_gid: None,
        },
    }
}

fn argv_values(command: &siderostat::cluster::Ds4Command) -> Vec<String> {
    command
        .argv
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}

/// 受入 case 1: 空白/日本語 path → 単一 argv。空白や日本語を含む model path が
/// 分割されず単一の argv 要素になる。。。
#[test]
fn whitespace_and_japanese_path_remains_a_single_argv() {
    let mut config = base_config();
    config.distributed.model = PathBuf::from("/models/DeepSeek V4 日本語.gguf");
    let command =
        build_tp_worker_command(&config, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 9911).unwrap();
    // model path が単一要素として存在する。
    let model_index = command
        .argv
        .iter()
        .position(|a| a == OsStr::new("-m"))
        .unwrap();
    assert_eq!(
        command.argv[model_index + 1],
        OsStr::new("/models/DeepSeek V4 日本語.gguf")
    );
    // 空白で分割されていない（argv 要素数が増えていない）。
    let values = argv_values(&command);
    assert_eq!(
        values
            .iter()
            .filter(|v| *v == "/models/DeepSeek V4 日本語.gguf")
            .count(),
        1
    );
}

/// 受入 case 2a: TP worker → --tensor-parallel / --role / --transport 各 1 回。
/// worker に HTTP 公開引数（--host / --port）と --layers がない。。。
#[test]
fn tp_worker_argv_matches_contract_without_http_args() {
    let config = base_config();
    let command =
        build_tp_worker_command(&config, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 9911).unwrap();
    let values = argv_values(&command);
    for flag in ["--tensor-parallel", "--role", "--transport"] {
        assert_eq!(
            values.iter().filter(|v| *v == flag).count(),
            1,
            "{flag} must appear exactly once"
        );
    }
    // worker に --host / --port / --layers がない。
    for flag in ["--host", "--port", "--layers", "--listen", "--debug"] {
        assert!(
            !values.contains(&flag.to_string()),
            "worker must not contain {flag}: {values:?}"
        );
    }
    // --coordinator HOST PORT が必須。
    let coord_index = values.iter().position(|v| v == "--coordinator").unwrap();
    assert_eq!(values[coord_index + 1], "10.99.0.1");
    assert_eq!(values[coord_index + 2], "9911");
}

/// 受入 case 2b: TP coordinator → --tensor-parallel / --role / --transport 各 1 回。
/// coordinator だけ HTTP 公開引数（--host / --port）を持ち、--layers がない。。。
#[test]
fn tp_coordinator_argv_matches_contract_and_owns_http_port() {
    let config = base_config();
    let command =
        build_tp_coordinator_command(&config, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 9911)
            .unwrap();
    let values = argv_values(&command);
    for flag in ["--tensor-parallel", "--role", "--transport"] {
        assert_eq!(
            values.iter().filter(|v| *v == flag).count(),
            1,
            "{flag} must appear exactly once"
        );
    }
    // coordinator は --host / --port を持つ。
    assert!(values.contains(&"--host".to_string()));
    assert!(values.contains(&"--port".to_string()));
    // coordinator に --layers / --coordinator がない。
    for flag in ["--layers", "--coordinator", "--debug"] {
        assert!(
            !values.contains(&flag.to_string()),
            "coordinator must not contain {flag}: {values:?}"
        );
    }
    // --listen HOST PORT が必須。
    let listen_index = values.iter().position(|v| v == "--listen").unwrap();
    assert_eq!(values[listen_index + 1], "10.99.0.1");
    assert_eq!(values[listen_index + 2], "9911");
}

/// 受入 case 3: LP → 既存 golden 不変。LP の argv に --tensor-parallel がなく、
/// --layers があり、coordinator は HTTP 公開引数を持つ。。。
#[test]
fn lp_builder_keeps_existing_golden() {
    let mut config = base_config();
    config.distributed.topology = DistributedTopology::LayerParallel;
    config.distributed.transport = Transport::Tcp;
    config.distributed.coordinator_layers = Some("0:19".into());
    config.distributed.worker_layers = Some("20:output".into());
    config.distributed.extra_args = vec!["--debug".into()];

    let worker =
        build_distributed_worker_command(&config, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 9911)
            .unwrap();
    let worker_values = argv_values(&worker);
    assert!(!worker_values.contains(&"--tensor-parallel".to_string()));
    assert!(worker_values.contains(&"--layers".to_string()));
    assert!(worker_values.contains(&"--coordinator".to_string()));

    let coordinator = build_distributed_coordinator_command(
        &config,
        IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)),
        9911,
    )
    .unwrap();
    let coordinator_values = argv_values(&coordinator);
    assert!(!coordinator_values.contains(&"--tensor-parallel".to_string()));
    assert!(coordinator_values.contains(&"--layers".to_string()));
    assert!(coordinator_values.contains(&"--listen".to_string()));
    // LP coordinator は HTTP 公開引数を持つ（既存 golden 不変）。。
    assert!(coordinator_values.contains(&"--host".to_string()));
    assert!(coordinator_values.contains(&"--port".to_string()));
}

/// 受入 case 4: source 未対応 flag → spawn 前拒否。TP で --debug / --layers /
/// --transport override を builder が拒否する。。。
#[test]
fn source_unsupported_flag_is_rejected_before_spawn() {
    // --debug は TP で未対応（A02 cli_validation: distributed debug options cannot be used）。
    let mut config = base_config();
    config.distributed.extra_args = vec!["--debug".into()];
    assert!(matches!(
        build_tp_worker_command(&config, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 9911)
            .unwrap_err(),
        Ds4CommandError::TpManagedArgumentOverride(_)
    ));

    // --layers は TP で禁止。
    let mut config2 = base_config();
    config2.distributed.extra_args = vec!["--layers".into()];
    assert!(matches!(
        build_tp_coordinator_command(&config2, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 9911)
            .unwrap_err(),
        Ds4CommandError::TpManagedArgumentOverride(_)
    ));

    // --transport override は TP で禁止。
    let mut config3 = base_config();
    config3.distributed.extra_args = vec!["--transport=auto".into()];
    assert!(matches!(
        build_tp_worker_command(&config3, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 9911)
            .unwrap_err(),
        Ds4CommandError::TpManagedArgumentOverride(_)
    ));
}

/// 事後条件: TP に --layers がない（受入 case 2 で確認済み）。working directory を
/// 保持する。。。
#[test]
fn tp_command_keeps_working_directory() {
    let config = base_config();
    let command =
        build_tp_worker_command(&config, IpAddr::V4(Ipv4Addr::new(10, 99, 0, 1)), 9911).unwrap();
    assert_eq!(command.working_directory, PathBuf::from("/work/ds4"));
    assert_eq!(
        command.executable,
        PathBuf::from("/usr/local/bin/ds4-server")
    );
}
