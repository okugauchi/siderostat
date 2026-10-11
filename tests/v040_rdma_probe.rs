//! T05 — RDMA read-only capability probe の受入 case。。。。。。
#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    RdmaCommandRunner, RdmaDeviceInfo, RdmaProbe, RdmaProbeError, RdmaProbeRequest,
};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// read-only コマンド runner の mock。テストで tool / IPv4 / device を制御する。。
struct MockRunner {
    tool: bool,
    ipv4_result: Result<Vec<IpAddr>, RdmaProbeError>,
    devices_result: Result<Vec<RdmaDeviceInfo>, RdmaProbeError>,
}

impl MockRunner {
    fn ready() -> Self {
        Self {
            tool: true,
            ipv4_result: Ok(vec![IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2))]),
            devices_result: Ok(vec![RdmaDeviceInfo {
                name: "mlx5_0".into(),
                gid_index: 3,
                ipv4_mapped_gid: Some(IpAddr::V6(Ipv6Addr::new(
                    0, 0, 0, 0, 0, 0xffff, 0x0a63, 0x0002,
                ))),
            }]),
        }
    }
}

impl RdmaCommandRunner for MockRunner {
    fn member_ipv4(&self, _interface: &str) -> Result<Vec<IpAddr>, RdmaProbeError> {
        self.ipv4_result.clone()
    }
    fn devices(&self) -> Result<Vec<RdmaDeviceInfo>, RdmaProbeError> {
        self.devices_result.clone()
    }
    fn tool_available(&self) -> bool {
        self.tool
    }
}

fn request(
    runner: &MockRunner,
    epoch: u64,
    interface: Option<&str>,
) -> Result<siderostat::cluster::RdmaObservation, RdmaProbeError> {
    let probe = RdmaProbe::new(Box::new(MockRunner {
        tool: runner.tool,
        ipv4_result: runner.ipv4_result.clone(),
        devices_result: runner.devices_result.clone(),
    }));
    probe.probe(&RdmaProbeRequest {
        epoch,
        current_epoch: 1,
        member_interface: interface.map(|s| s.to_string()),
        member_address: None,
        device: None,
        gid_index: None,
        deadline: Duration::from_secs(5),
    })
}

/// 受入 case 1: bridge だけ IPv4 → MissingMemberAddress。bridge IP は member として不可。。
#[test]
fn bridge_only_ipv4_is_missing_member_address() {
    let runner = MockRunner::ready();
    // member_interface が bridge0 → MissingMemberAddress。。
    let err = request(&runner, 1, Some("bridge0")).unwrap_err();
    assert_eq!(err, RdmaProbeError::MissingMemberAddress);
}

/// 受入 case 2: 複数 GID で指定なし → AmbiguousDevice。。
#[test]
fn multiple_gids_without_selection_is_ambiguous_device() {
    let runner = MockRunner {
        tool: true,
        ipv4_result: Ok(vec![IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2))]),
        devices_result: Ok(vec![
            RdmaDeviceInfo {
                name: "mlx5_0".into(),
                gid_index: 1,
                ipv4_mapped_gid: Some(IpAddr::V6(Ipv6Addr::new(
                    0, 0, 0, 0, 0, 0xffff, 0x0a63, 0x0001,
                ))),
            },
            RdmaDeviceInfo {
                name: "mlx5_1".into(),
                gid_index: 2,
                ipv4_mapped_gid: Some(IpAddr::V6(Ipv6Addr::new(
                    0, 0, 0, 0, 0, 0xffff, 0x0a63, 0x0002,
                ))),
            },
        ]),
    };
    // device / gid_index 指定なし → AmbiguousDevice。。
    let err = request(&runner, 1, Some("enp5s0")).unwrap_err();
    assert_eq!(err, RdmaProbeError::AmbiguousDevice);
}

/// 受入 case 3a: tool 無し → Unavailable。。
#[test]
fn missing_tool_is_unavailable() {
    let runner = MockRunner {
        tool: false,
        ..MockRunner::ready()
    };
    let err = request(&runner, 1, Some("enp5s0")).unwrap_err();
    assert_eq!(err, RdmaProbeError::Unavailable);
}

/// 受入 case 3b: 権限拒否 → Unknown。read-only で失敗し、分類不能は Unknown。。
#[test]
fn permission_denied_is_unknown() {
    let runner = MockRunner {
        tool: true,
        ipv4_result: Err(RdmaProbeError::Unknown),
        devices_result: Ok(vec![]),
    };
    let err = request(&runner, 1, Some("enp5s0")).unwrap_err();
    assert_eq!(err, RdmaProbeError::Unknown);
}

/// 受入 case 4: stale epoch → 拒否。観測 epoch が現在 epoch より小さい場合は StaleEpoch。。
#[test]
fn stale_epoch_is_rejected() {
    let runner = MockRunner::ready();
    // epoch=0、current_epoch=1 → stale。。
    let err = request(&runner, 0, Some("enp5s0")).unwrap_err();
    assert_eq!(err, RdmaProbeError::StaleEpoch);
}

/// 事後条件: ping や bridge IP のみでは ReadyCandidate にならない。適切な member IPv4 +
/// active device + IPv4-mapped GID が揃った場合のみ ready_candidate=true。。
#[test]
fn ready_candidate_requires_full_rdma_evidence() {
    let runner = MockRunner::ready();
    let observation = request(&runner, 1, Some("enp5s0")).unwrap();
    assert!(observation.ready_candidate);
    assert_eq!(observation.device.as_deref(), Some("mlx5_0"));
    // member IPv4 が member interface のものである。。
    assert_eq!(
        observation.member_ipv4,
        Some(IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2)))
    );
    // IPv4-mapped GID。。
    assert!(observation.gid.unwrap().starts_with("::ffff:"));
}
