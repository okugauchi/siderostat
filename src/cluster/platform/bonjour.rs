use crate::cluster::{BonjourFailure, BonjourRegistration};
use libc::{AF_INET, sockaddr, sockaddr_in};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_uchar, c_void},
    io,
    net::Ipv4Addr,
    os::fd::{AsRawFd, RawFd},
    ptr::{self, NonNull},
};
use tokio::{io::unix::AsyncFd, sync::mpsc};

type DnsServiceRef = *mut c_void;
type DnsServiceFlags = u32;
type DnsServiceError = i32;

const NO_ERROR: DnsServiceError = 0;
const FLAG_ADD: DnsServiceFlags = 0x2;
const PROTOCOL_IPV4: u32 = 0x1;

unsafe extern "C" {
    fn DNSServiceRegister(
        service: *mut DnsServiceRef,
        flags: DnsServiceFlags,
        interface_index: u32,
        name: *const c_char,
        service_type: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_len: u16,
        txt_record: *const c_void,
        callback: Option<RegisterCallback>,
        context: *mut c_void,
    ) -> DnsServiceError;
    fn DNSServiceBrowse(
        service: *mut DnsServiceRef,
        flags: DnsServiceFlags,
        interface_index: u32,
        service_type: *const c_char,
        domain: *const c_char,
        callback: Option<BrowseCallback>,
        context: *mut c_void,
    ) -> DnsServiceError;
    fn DNSServiceResolve(
        service: *mut DnsServiceRef,
        flags: DnsServiceFlags,
        interface_index: u32,
        name: *const c_char,
        service_type: *const c_char,
        domain: *const c_char,
        callback: Option<ResolveCallback>,
        context: *mut c_void,
    ) -> DnsServiceError;
    fn DNSServiceGetAddrInfo(
        service: *mut DnsServiceRef,
        flags: DnsServiceFlags,
        interface_index: u32,
        protocol: u32,
        hostname: *const c_char,
        callback: Option<AddrInfoCallback>,
        context: *mut c_void,
    ) -> DnsServiceError;
    fn DNSServiceRefSockFD(service: DnsServiceRef) -> c_int;
    fn DNSServiceProcessResult(service: DnsServiceRef) -> DnsServiceError;
    fn DNSServiceRefDeallocate(service: DnsServiceRef);
}

type RegisterCallback = unsafe extern "C" fn(
    DnsServiceRef,
    DnsServiceFlags,
    DnsServiceError,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type BrowseCallback = unsafe extern "C" fn(
    DnsServiceRef,
    DnsServiceFlags,
    u32,
    DnsServiceError,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type ResolveCallback = unsafe extern "C" fn(
    DnsServiceRef,
    DnsServiceFlags,
    u32,
    DnsServiceError,
    *const c_char,
    *const c_char,
    u16,
    u16,
    *const c_uchar,
    *mut c_void,
);
type AddrInfoCallback = unsafe extern "C" fn(
    DnsServiceRef,
    DnsServiceFlags,
    u32,
    DnsServiceError,
    *const c_char,
    *const sockaddr,
    u32,
    *mut c_void,
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BonjourPlatformEvent {
    Registered {
        generation: u64,
    },
    ServiceAdded {
        generation: u64,
        interface_index: u32,
        name: String,
        service_type: String,
        domain: String,
    },
    ServiceRemoved {
        generation: u64,
        interface_index: u32,
        name: String,
    },
    ServiceResolved {
        generation: u64,
        interface_index: u32,
        host_target: String,
        port: u16,
        protocol_version: Option<u16>,
        node_id: Option<String>,
    },
    AddressResolved {
        generation: u64,
        interface_index: u32,
        hostname: String,
        address: Ipv4Addr,
        added: bool,
    },
    Failed {
        generation: u64,
        failure: BonjourFailure,
    },
}

struct CallbackContext {
    generation: u64,
    events: mpsc::Sender<BonjourPlatformEvent>,
}

struct DnsSocket(RawFd);

impl AsRawFd for DnsSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

pub struct MacOsBonjourOperation {
    generation: u64,
    service: NonNull<c_void>,
    socket: AsyncFd<DnsSocket>,
    _context: Box<CallbackContext>,
}

// SAFETY: DNSServiceRef is only processed through `&mut self`; callbacks use a bounded Tokio
// sender, and `_context` keeps callback state alive for the lifetime of the operation.
unsafe impl Send for MacOsBonjourOperation {}

pub fn bridge0_interface_index() -> io::Result<u32> {
    // SAFETY: the byte string is statically NUL-terminated.
    let index = unsafe { libc::if_nametoindex(c"bridge0".as_ptr()) };
    if index == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(index)
    }
}

impl MacOsBonjourOperation {
    pub fn register(
        registration: &BonjourRegistration,
        events: mpsc::Sender<BonjourPlatformEvent>,
    ) -> io::Result<Self> {
        let name = c_string(&registration.node_id)?;
        let service_type = c_string(&registration.service_type)?;
        let domain = c_string(&registration.domain)?;
        let txt = txt_record(registration.protocol_version, &registration.node_id)?;
        let context = Box::new(CallbackContext {
            generation: registration.generation,
            events,
        });
        let mut service = ptr::null_mut();
        // SAFETY: CString/TXT buffers and boxed callback context live through this call; the
        // returned operation retains the context until after DNSServiceRef deallocation.
        let error = unsafe {
            DNSServiceRegister(
                &mut service,
                0,
                registration.interface_index,
                name.as_ptr(),
                service_type.as_ptr(),
                domain.as_ptr(),
                ptr::null(),
                registration.port_network_order,
                txt.len() as u16,
                txt.as_ptr().cast(),
                Some(register_callback),
                (&*context as *const CallbackContext).cast_mut().cast(),
            )
        };
        Self::finish_start(registration.generation, service, error, context)
    }

    pub fn browse(
        registration: &BonjourRegistration,
        events: mpsc::Sender<BonjourPlatformEvent>,
    ) -> io::Result<Self> {
        let service_type = c_string(&registration.service_type)?;
        let domain = c_string(&registration.domain)?;
        let context = Box::new(CallbackContext {
            generation: registration.generation,
            events,
        });
        let mut service = ptr::null_mut();
        // SAFETY: CString buffers cover the synchronous call and the boxed context is retained.
        let error = unsafe {
            DNSServiceBrowse(
                &mut service,
                0,
                registration.interface_index,
                service_type.as_ptr(),
                domain.as_ptr(),
                Some(browse_callback),
                (&*context as *const CallbackContext).cast_mut().cast(),
            )
        };
        Self::finish_start(registration.generation, service, error, context)
    }

    pub fn resolve(
        registration: &BonjourRegistration,
        name: &str,
        events: mpsc::Sender<BonjourPlatformEvent>,
    ) -> io::Result<Self> {
        let name = c_string(name)?;
        let service_type = c_string(&registration.service_type)?;
        let domain = c_string(&registration.domain)?;
        let context = Box::new(CallbackContext {
            generation: registration.generation,
            events,
        });
        let mut service = ptr::null_mut();
        // SAFETY: CString buffers cover the synchronous call and the boxed context is retained.
        let error = unsafe {
            DNSServiceResolve(
                &mut service,
                0,
                registration.interface_index,
                name.as_ptr(),
                service_type.as_ptr(),
                domain.as_ptr(),
                Some(resolve_callback),
                (&*context as *const CallbackContext).cast_mut().cast(),
            )
        };
        Self::finish_start(registration.generation, service, error, context)
    }

    pub fn get_addr_info(
        generation: u64,
        interface_index: u32,
        hostname: &str,
        events: mpsc::Sender<BonjourPlatformEvent>,
    ) -> io::Result<Self> {
        if interface_index == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DNS-SD interface index must be nonzero",
            ));
        }
        let hostname = c_string(hostname)?;
        let context = Box::new(CallbackContext { generation, events });
        let mut service = ptr::null_mut();
        // SAFETY: hostname covers the synchronous call and the boxed context is retained.
        let error = unsafe {
            DNSServiceGetAddrInfo(
                &mut service,
                0,
                interface_index,
                PROTOCOL_IPV4,
                hostname.as_ptr(),
                Some(addr_info_callback),
                (&*context as *const CallbackContext).cast_mut().cast(),
            )
        };
        Self::finish_start(generation, service, error, context)
    }

    fn finish_start(
        generation: u64,
        service: DnsServiceRef,
        error: DnsServiceError,
        context: Box<CallbackContext>,
    ) -> io::Result<Self> {
        if error != NO_ERROR {
            return Err(io::Error::other(format!("DNS-SD start failed: {error}")));
        }
        let service = NonNull::new(service)
            .ok_or_else(|| io::Error::other("DNS-SD returned a null service reference"))?;
        // SAFETY: successful DNSServiceRegister/Browse returns a live reference.
        let fd = unsafe { DNSServiceRefSockFD(service.as_ptr()) };
        if fd < 0 {
            // SAFETY: this is the sole owner of the successfully-created reference.
            unsafe { DNSServiceRefDeallocate(service.as_ptr()) };
            return Err(io::Error::other("DNS-SD returned an invalid socket"));
        }
        let socket = match AsyncFd::new(DnsSocket(fd)) {
            Ok(socket) => socket,
            Err(error) => {
                // SAFETY: construction failed before ownership could be transferred.
                unsafe { DNSServiceRefDeallocate(service.as_ptr()) };
                return Err(error);
            }
        };
        Ok(Self {
            generation,
            service,
            socket,
            _context: context,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub async fn process_next(&mut self) -> io::Result<()> {
        let mut ready = self.socket.readable().await?;
        // SAFETY: `self` exclusively owns the reference and serializes result processing.
        let error = unsafe { DNSServiceProcessResult(self.service.as_ptr()) };
        ready.clear_ready();
        if error == NO_ERROR {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "DNSServiceProcessResult failed: {error}"
            )))
        }
    }
}

impl Drop for MacOsBonjourOperation {
    fn drop(&mut self) {
        // SAFETY: this operation is the sole owner and deallocates exactly once.
        unsafe { DNSServiceRefDeallocate(self.service.as_ptr()) };
    }
}

unsafe extern "C" fn register_callback(
    _service: DnsServiceRef,
    _flags: DnsServiceFlags,
    error: DnsServiceError,
    _name: *const c_char,
    _service_type: *const c_char,
    _domain: *const c_char,
    raw_context: *mut c_void,
) {
    // SAFETY: context is owned by the live operation for the callback lifetime.
    let context = unsafe { &*(raw_context.cast::<CallbackContext>()) };
    let event = if error == NO_ERROR {
        BonjourPlatformEvent::Registered {
            generation: context.generation,
        }
    } else {
        BonjourPlatformEvent::Failed {
            generation: context.generation,
            failure: map_failure(error),
        }
    };
    let _ = context.events.try_send(event);
}

unsafe extern "C" fn browse_callback(
    _service: DnsServiceRef,
    flags: DnsServiceFlags,
    interface_index: u32,
    error: DnsServiceError,
    name: *const c_char,
    service_type: *const c_char,
    domain: *const c_char,
    raw_context: *mut c_void,
) {
    // SAFETY: context and C strings are provided by DNS-SD for this callback invocation.
    let context = unsafe { &*(raw_context.cast::<CallbackContext>()) };
    let event = if error != NO_ERROR {
        BonjourPlatformEvent::Failed {
            generation: context.generation,
            failure: map_failure(error),
        }
    } else if flags & FLAG_ADD != 0 {
        BonjourPlatformEvent::ServiceAdded {
            generation: context.generation,
            interface_index,
            name: c_str_lossy(name),
            service_type: c_str_lossy(service_type),
            domain: c_str_lossy(domain),
        }
    } else {
        BonjourPlatformEvent::ServiceRemoved {
            generation: context.generation,
            interface_index,
            name: c_str_lossy(name),
        }
    };
    let _ = context.events.try_send(event);
}

unsafe extern "C" fn resolve_callback(
    _service: DnsServiceRef,
    _flags: DnsServiceFlags,
    interface_index: u32,
    error: DnsServiceError,
    _fullname: *const c_char,
    host_target: *const c_char,
    port: u16,
    txt_len: u16,
    txt_record: *const c_uchar,
    raw_context: *mut c_void,
) {
    // SAFETY: context and callback buffers are valid for this invocation.
    let context = unsafe { &*(raw_context.cast::<CallbackContext>()) };
    let event = if error != NO_ERROR {
        BonjourPlatformEvent::Failed {
            generation: context.generation,
            failure: map_failure(error),
        }
    } else {
        // SAFETY: DNS-SD supplies `txt_len` readable bytes, or zero bytes with any pointer value.
        let txt = if txt_len == 0 {
            &[]
        } else if txt_record.is_null() {
            return;
        } else {
            // SAFETY: DNS-SD supplies `txt_len` readable bytes for a non-null TXT record during
            // this callback.
            unsafe { std::slice::from_raw_parts(txt_record, usize::from(txt_len)) }
        };
        let (protocol_version, node_id) = parse_txt_record(txt);
        BonjourPlatformEvent::ServiceResolved {
            generation: context.generation,
            interface_index,
            host_target: c_str_lossy(host_target),
            port: u16::from_be(port),
            protocol_version,
            node_id,
        }
    };
    let _ = context.events.try_send(event);
}

unsafe extern "C" fn addr_info_callback(
    _service: DnsServiceRef,
    flags: DnsServiceFlags,
    interface_index: u32,
    error: DnsServiceError,
    hostname: *const c_char,
    address: *const sockaddr,
    _ttl: u32,
    raw_context: *mut c_void,
) {
    // SAFETY: context is owned by the live operation for the callback lifetime.
    let context = unsafe { &*(raw_context.cast::<CallbackContext>()) };
    let event = if error != NO_ERROR {
        BonjourPlatformEvent::Failed {
            generation: context.generation,
            failure: map_failure(error),
        }
    } else {
        let Some(address) = ipv4_from_sockaddr(address) else {
            return;
        };
        BonjourPlatformEvent::AddressResolved {
            generation: context.generation,
            interface_index,
            hostname: c_str_lossy(hostname),
            address,
            added: flags & FLAG_ADD != 0,
        }
    };
    let _ = context.events.try_send(event);
}

fn c_string(value: &str) -> io::Result<CString> {
    CString::new(value).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interior NUL"))
}

fn txt_record(protocol: u16, node_id: &str) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    for item in [format!("protocol={protocol}"), format!("node_id={node_id}")] {
        let bytes = item.as_bytes();
        let length = u8::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "TXT item too long"))?;
        output.push(length);
        output.extend_from_slice(bytes);
    }
    Ok(output)
}

fn parse_txt_record(record: &[u8]) -> (Option<u16>, Option<String>) {
    let mut protocol = None;
    let mut node_id = None;
    let mut remaining = record;
    while let Some((&length, tail)) = remaining.split_first() {
        let length = usize::from(length);
        if tail.len() < length {
            break;
        }
        let (item, next) = tail.split_at(length);
        if let Some(value) = item.strip_prefix(b"protocol=") {
            protocol = std::str::from_utf8(value).ok().and_then(|v| v.parse().ok());
        } else if let Some(value) = item.strip_prefix(b"node_id=") {
            node_id = std::str::from_utf8(value).ok().map(ToOwned::to_owned);
        }
        remaining = next;
    }
    (protocol, node_id)
}

fn ipv4_from_sockaddr(address: *const sockaddr) -> Option<Ipv4Addr> {
    if address.is_null() {
        return None;
    }
    // SAFETY: the family byte is part of every sockaddr supplied by DNS-SD.
    let family = unsafe { (*address).sa_family as c_int };
    if family != AF_INET {
        return None;
    }
    // SAFETY: AF_INET identifies the pointed-to value as sockaddr_in.
    let address = unsafe { &*address.cast::<sockaddr_in>() };
    Some(Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()))
}

fn c_str_lossy(value: *const c_char) -> String {
    if value.is_null() {
        return String::new();
    }
    // SAFETY: DNS-SD callback string parameters are NUL-terminated for the callback lifetime.
    unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned()
}

fn map_failure(error: DnsServiceError) -> BonjourFailure {
    match error {
        -65563 => BonjourFailure::DaemonUnavailable,
        -65570 => BonjourFailure::PolicyDenied,
        -65571 => BonjourFailure::NotPermitted,
        value => BonjourFailure::Other(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_minimal_safe_txt_record_and_maps_permission_errors() {
        let txt = txt_record(1, "node-a").unwrap();
        assert!(txt.windows(b"protocol=1".len()).any(|v| v == b"protocol=1"));
        assert!(
            txt.windows(b"node_id=node-a".len())
                .any(|v| v == b"node_id=node-a")
        );
        assert_eq!(map_failure(-65571), BonjourFailure::NotPermitted);
        assert_eq!(map_failure(-65570), BonjourFailure::PolicyDenied);
        assert_eq!(map_failure(-65563), BonjourFailure::DaemonUnavailable);
        assert_eq!(parse_txt_record(&txt), (Some(1), Some("node-a".to_owned())));
        assert_eq!(parse_txt_record(&[5, b'a']), (None, None));
    }
}
