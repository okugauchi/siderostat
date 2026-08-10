#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BonjourFailure {
    NotPermitted,
    PolicyDenied,
    DaemonUnavailable,
    RegistrationFailed,
    Other(i32),
}

impl BonjourFailure {
    pub fn allows_static_fallback(self) -> bool {
        matches!(
            self,
            Self::NotPermitted
                | Self::PolicyDenied
                | Self::DaemonUnavailable
                | Self::RegistrationFailed
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BonjourRegistration {
    pub generation: u64,
    pub interface_index: u32,
    pub service_type: String,
    pub domain: String,
    pub port_network_order: u16,
    pub protocol_version: u16,
    pub node_id: String,
}

impl BonjourRegistration {
    pub fn new(
        generation: u64,
        interface_index: u32,
        service_type: impl Into<String>,
        domain: impl Into<String>,
        port: u16,
        node_id: impl Into<String>,
    ) -> Option<Self> {
        (interface_index != 0 && port != 0).then(|| Self {
            generation,
            interface_index,
            service_type: service_type.into(),
            domain: domain.into(),
            port_network_order: port.to_be(),
            protocol_version: 1,
            node_id: node_id.into(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BonjourLifecycle {
    generation: u64,
    active: bool,
}

impl BonjourLifecycle {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            active: true,
        }
    }

    pub fn accepts(&self, generation: u64) -> bool {
        self.active && self.generation == generation
    }

    pub fn invalidate(&mut self) {
        self.active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_uses_network_byte_order_and_generation_lifecycle() {
        let registration =
            BonjourRegistration::new(9, 13, "_ds4cluster._tcp", "local.", 9920, "node-a").unwrap();
        assert_eq!(registration.port_network_order, 9920_u16.to_be());
        let mut lifecycle = BonjourLifecycle::new(9);
        assert!(lifecycle.accepts(9));
        assert!(!lifecycle.accepts(8));
        lifecycle.invalidate();
        assert!(!lifecycle.accepts(9));
    }
}
