//! v0.4.0 RDMA read-only capability probe（C02 / T05）。
//!
//! member interface の IPv4 / GID / device / route / OS を read-only で読み取り、
//! TP の RDMA 準備状況を観測する。sudo / ifconfig 変更 / rdma_ctl enable を自動で
//! 行わない（OS 設定変更は H01 の人間 packet に集約）。ping や bridge IP のみでは
//! ReadyCandidate にならない。probe 失敗は Unavailable / Unsupported / Unknown に分類する。
//!
//! 世代契約（C02: TP session / control generation / cluster generation / policy epoch は
//! 別型）に従い、観測結果に epoch を持たせ、stale epoch の観測を拒否する。

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

/// RDMA probe の観測結果。epoch を持ち、stale な観測は拒否する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdmaObservation {
    /// 観測の世代。stale な観測は拒否する。
    pub epoch: u64,
    pub member_interface: Option<String>,
    /// member インターフェースの IPv4。bridge IP は不可。
    pub member_ipv4: Option<IpAddr>,
    pub device: Option<String>,
    pub gid_index: Option<u32>,
    /// IPv4-mapped GID（例: ::ffff:10.99.0.2）。
    pub gid: Option<String>,
    /// ping や bridge IP のみでは true にならない。member IPv4 + active device +
    /// IPv4-mapped GID が揃った場合のみ。
    pub ready_candidate: bool,
}

/// probe の失敗分類。Unavailable（tool なし）/ Unsupported（device/GID 不適合）/
/// Unknown（権限拒否等）に分類する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdmaProbeError {
    /// member に IPv4 がない、または bridge だけ IPv4（bridge IP は不可）。
    MissingMemberAddress,
    /// 複数の device/GID があり指定がない。
    AmbiguousDevice,
    /// 必要な tool（rdma_ctl / ibv_devinfo）がない。
    Unavailable,
    /// device / GID が RDMA に不適合（IPv4-mapped GID がない等）。
    Unsupported,
    /// 権限拒否等の不明な失敗。
    Unknown,
    /// stale epoch の観測。
    StaleEpoch,
}

impl fmt::Display for RdmaProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            RdmaProbeError::MissingMemberAddress => {
                "member interface has no usable IPv4 (bridge IP is not usable)"
            }
            RdmaProbeError::AmbiguousDevice => {
                "multiple RDMA devices/GIDs present without an explicit selection"
            }
            RdmaProbeError::Unavailable => "RDMA probe tool is unavailable",
            RdmaProbeError::Unsupported => "RDMA device/GID is unsupported (no IPv4-mapped GID)",
            RdmaProbeError::Unknown => "RDMA probe failed with an unknown error",
            RdmaProbeError::StaleEpoch => "RDMA observation epoch is stale",
        };
        f.write_str(message)
    }
}

/// probe の入力。member の IPv4 / GID / device を read-only で読み取る。
#[derive(Debug, Clone)]
pub struct RdmaProbeRequest {
    pub epoch: u64,
    /// 現在の epoch。観測 epoch がこれより小さい場合は StaleEpoch。世代契約。
    pub current_epoch: u64,
    pub member_interface: Option<String>,
    /// rdma_address。bridge の IPv4 に一致する場合は MissingMemberAddress。。
    pub member_address: Option<IpAddr>,
    pub device: Option<String>,
    pub gid_index: Option<u32>,
    pub deadline: Duration,
}

/// コマンド実行 adapter。read-only。テストでは mock する。
pub trait RdmaCommandRunner: Send + Sync {
    /// `ifconfig <interface>` を実行し、IPv4 アドレス行を返す（read-only）。
    fn member_ipv4(&self, interface: &str) -> Result<Vec<IpAddr>, RdmaProbeError>;
    /// `rdma_ctl status` / `ibv_devinfo -v` を実行し、device/GID 情報を返す（read-only）。
    fn devices(&self) -> Result<Vec<RdmaDeviceInfo>, RdmaProbeError>;
    /// tool が存在するか。存在しない場合は Unavailable。
    fn tool_available(&self) -> bool;
}

/// 検出した RDMA device の情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdmaDeviceInfo {
    pub name: String,
    pub gid_index: u32,
    /// IPv4-mapped GID（例: ::ffff:10.99.0.2）。IPv4-mapped でなければ None。
    pub ipv4_mapped_gid: Option<IpAddr>,
}

/// bridge インターフェース判定。`bridge` prefix の interface は member として不可。
fn is_bridge_interface(name: &str) -> bool {
    name.starts_with("bridge") || name.starts_with("br0") || name.starts_with("en0")
}

/// member IPv4 を判定する。bridge だけ IPv4 の場合は MissingMemberAddress。
fn resolve_member_ipv4(interface: &str, addresses: &[IpAddr]) -> Result<IpAddr, RdmaProbeError> {
    if addresses.is_empty() {
        return Err(RdmaProbeError::MissingMemberAddress);
    }
    // bridge interface の IPv4 は member として使えない（bridge IP は不可）。
    if is_bridge_interface(interface) {
        return Err(RdmaProbeError::MissingMemberAddress);
    }
    // member interface の IPv4 を返す。
    addresses
        .first()
        .copied()
        .ok_or(RdmaProbeError::MissingMemberAddress)
}

/// device / GID を判定する。複数の IPv4-mapped GID があり指定がない場合は AmbiguousDevice。
fn resolve_device(
    devices: &[RdmaDeviceInfo],
    device: Option<&str>,
    gid_index: Option<u32>,
) -> Result<(String, u32, IpAddr), RdmaProbeError> {
    if devices.is_empty() {
        return Err(RdmaProbeError::Unsupported);
    }
    let candidates: Vec<&RdmaDeviceInfo> = devices
        .iter()
        .filter(|d| {
            d.ipv4_mapped_gid.is_some()
                && device.is_none_or(|name| d.name == name)
                && gid_index.is_none_or(|index| d.gid_index == index)
        })
        .collect();
    if candidates.is_empty() {
        return Err(RdmaProbeError::Unsupported);
    }
    if candidates.len() > 1 {
        return Err(RdmaProbeError::AmbiguousDevice);
    }
    let chosen = candidates[0];
    let gid = chosen.ipv4_mapped_gid.ok_or(RdmaProbeError::Unsupported)?;
    Ok((chosen.name.clone(), chosen.gid_index, gid))
}

/// RDMA probe。read-only で member IPv4 / device / GID を観測する。
pub struct RdmaProbe {
    runner: Box<dyn RdmaCommandRunner>,
}

impl RdmaProbe {
    pub fn new(runner: Box<dyn RdmaCommandRunner>) -> Self {
        Self { runner }
    }

    pub fn probe(&self, request: &RdmaProbeRequest) -> Result<RdmaObservation, RdmaProbeError> {
        // 世代契約: 観測 epoch が現在 epoch より小さい場合は stale として拒否する。。
        if is_stale_epoch(request.epoch, request.current_epoch) {
            return Err(RdmaProbeError::StaleEpoch);
        }
        if !self.runner.tool_available() {
            return Err(RdmaProbeError::Unavailable);
        }
        let interface = request
            .member_interface
            .as_deref()
            .ok_or(RdmaProbeError::MissingMemberAddress)?;
        let addresses = self.runner.member_ipv4(interface)?;
        let member_ipv4 = resolve_member_ipv4(interface, &addresses)?;
        let devices = self.runner.devices()?;
        let (device_name, gid_index, gid) =
            resolve_device(&devices, request.device.as_deref(), request.gid_index)?;
        Ok(RdmaObservation {
            epoch: request.epoch,
            member_interface: Some(interface.to_string()),
            member_ipv4: Some(member_ipv4),
            device: Some(device_name),
            gid_index: Some(gid_index),
            gid: Some(gid.to_string()),
            ready_candidate: true,
        })
    }
}

/// epoch の stale 判定。観測 epoch が current より小さい場合は stale。。
/// 単一 epoch 空間ではないため、呼び出し側が current epoch を渡す。
pub fn is_stale_epoch(observed: u64, current: u64) -> bool {
    observed < current
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const MEMBER: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 99, 0, 2));

    #[test]
    fn member_ipv4_requires_non_bridge_interface() {
        let addresses = vec![MEMBER];
        assert_eq!(resolve_member_ipv4("enp5s0", &addresses).unwrap(), MEMBER);
        // bridge0 は member として不可。。
        assert_eq!(
            resolve_member_ipv4("bridge0", &addresses),
            Err(RdmaProbeError::MissingMemberAddress)
        );
        // アドレスなしも MissingMemberAddress。。
        assert_eq!(
            resolve_member_ipv4("enp5s0", &[]),
            Err(RdmaProbeError::MissingMemberAddress)
        );
    }

    fn ipv4_mapped(host: u16) -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0a63, host))
    }

    #[test]
    fn device_resolution_requires_single_candidate() {
        let devices = vec![
            RdmaDeviceInfo {
                name: "mlx5_0".into(),
                gid_index: 1,
                ipv4_mapped_gid: Some(ipv4_mapped(1)),
            },
            RdmaDeviceInfo {
                name: "mlx5_1".into(),
                gid_index: 2,
                ipv4_mapped_gid: Some(ipv4_mapped(2)),
            },
        ];
        // 指定なし → AmbiguousDevice。。
        assert_eq!(
            resolve_device(&devices, None, None),
            Err(RdmaProbeError::AmbiguousDevice)
        );
        // device 指定 → 一意。。
        let (name, index, gid) = resolve_device(&devices, Some("mlx5_0"), None).unwrap();
        assert_eq!(name, "mlx5_0");
        assert_eq!(index, 1);
        assert_eq!(gid, ipv4_mapped(1));
        // IPv4-mapped GID がない → Unsupported。。
        let no_gid = vec![RdmaDeviceInfo {
            name: "mlx5_0".into(),
            gid_index: 0,
            ipv4_mapped_gid: None,
        }];
        assert_eq!(
            resolve_device(&no_gid, None, None),
            Err(RdmaProbeError::Unsupported)
        );
        // device なし → Unsupported。。
        assert_eq!(
            resolve_device(&[], None, None),
            Err(RdmaProbeError::Unsupported)
        );
    }

    #[test]
    fn stale_epoch_compares_observed_to_current() {
        assert!(is_stale_epoch(0, 1));
        assert!(is_stale_epoch(41, 42));
        assert!(!is_stale_epoch(42, 42));
        assert!(!is_stale_epoch(43, 42));
    }

    #[test]
    fn probe_reads_member_device_and_gid_read_only() {
        let probe = RdmaProbe::new(Box::new(TestRunner));
        let observation = probe
            .probe(&RdmaProbeRequest {
                epoch: 1,
                current_epoch: 1,
                member_interface: Some("enp5s0".into()),
                member_address: None,
                device: None,
                gid_index: None,
                deadline: Duration::from_secs(5),
            })
            .unwrap();
        assert!(observation.ready_candidate);
        assert_eq!(observation.member_ipv4, Some(MEMBER));
        assert_eq!(observation.device.as_deref(), Some("mlx5_0"));
        assert!(observation.gid.unwrap().starts_with("::ffff:"));
    }

    struct TestRunner;
    impl RdmaCommandRunner for TestRunner {
        fn member_ipv4(&self, _interface: &str) -> Result<Vec<IpAddr>, RdmaProbeError> {
            Ok(vec![MEMBER])
        }
        fn devices(&self) -> Result<Vec<RdmaDeviceInfo>, RdmaProbeError> {
            Ok(vec![RdmaDeviceInfo {
                name: "mlx5_0".into(),
                gid_index: 3,
                ipv4_mapped_gid: Some(ipv4_mapped(2)),
            }])
        }
        fn tool_available(&self) -> bool {
            true
        }
    }
}
