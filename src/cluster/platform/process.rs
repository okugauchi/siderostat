use crate::cluster::{ObservedProcess, ProcessInspector, ProcessSignal, ProcessSignaler};
use std::{
    ffi::{CStr, OsString, c_int, c_void},
    io,
    mem::{size_of, zeroed},
    os::unix::ffi::OsStringExt,
    path::PathBuf,
    ptr,
};

#[derive(Debug, Clone, Copy)]
pub struct MacOsProcessInspector;

impl ProcessInspector for MacOsProcessInspector {
    fn observe(&self, pid: u32) -> io::Result<Option<ObservedProcess>> {
        let pid = c_int::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PID exceeds c_int"))?;
        let executable = match process_path(pid) {
            Ok(path) => path,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(None),
            Err(error) => return Err(error),
        };
        let argv = process_argv(pid)?;
        let start_time_micros = process_start_time(pid)?;
        Ok(Some(ObservedProcess {
            pid: pid as u32,
            executable,
            argv,
            start_time_micros,
        }))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MacOsProcessSignaler;

impl ProcessSignaler for MacOsProcessSignaler {
    fn signal_process_group(&self, pid: u32, signal: ProcessSignal) -> io::Result<()> {
        signal_pid(pid, signal, true)
    }

    fn signal_process(&self, pid: u32, signal: ProcessSignal) -> io::Result<()> {
        signal_pid(pid, signal, false)
    }
}

fn signal_pid(pid: u32, signal: ProcessSignal, process_group: bool) -> io::Result<()> {
    let pid = c_int::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PID exceeds c_int"))?;
    let signal = match signal {
        ProcessSignal::Terminate => libc::SIGTERM,
        ProcessSignal::Kill => libc::SIGKILL,
    };
    let target = if process_group { -pid } else { pid };
    // SAFETY: the caller has verified the process identity immediately before signaling.
    if unsafe { libc::kill(target, signal) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Enumerate process identities visible to the current macOS user.
pub(crate) fn list_processes() -> io::Result<Vec<ObservedProcess>> {
    let mut pids = vec![0_i32; 1024];
    loop {
        let capacity = i32::try_from(pids.len().saturating_mul(size_of::<i32>()))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "process list too large"))?;
        // SAFETY: the buffer is writable for `capacity` bytes and contains i32 PID slots.
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), capacity) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = count as usize;
        if count >= pids.len() {
            pids.resize(pids.len().saturating_mul(2).max(count + 1), 0);
            continue;
        }
        pids.truncate(count);
        let inspector = MacOsProcessInspector;
        let mut processes = Vec::with_capacity(pids.len());
        for pid in pids.into_iter().filter(|pid| *pid > 0) {
            match inspector.observe(pid as u32) {
                Ok(Some(process)) => processes.push(process),
                Ok(None) => {}
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(code)
                            if matches!(
                                code,
                                libc::ENOENT
                                    | libc::ESRCH
                                    | libc::EPERM
                                    | libc::EACCES
                                    | libc::EIO
                                    | libc::EINVAL
                            )
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        return Ok(processes);
    }
}

fn process_path(pid: c_int) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buffer is writable for the advertised capacity.
    let length = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<c_void>(),
            buffer.len() as u32,
        )
    };
    if length <= 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(length as usize);
    Ok(PathBuf::from(OsString::from_vec(buffer)))
}

fn process_start_time(pid: c_int) -> io::Result<u64> {
    // SAFETY: zero is a valid initial bit pattern for proc_bsdinfo.
    let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
    // SAFETY: info points to a correctly-sized writable proc_bsdinfo.
    let length = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size_of::<libc::proc_bsdinfo>() as c_int,
        )
    };
    if length != size_of::<libc::proc_bsdinfo>() as c_int {
        return Err(io::Error::last_os_error());
    }
    Ok(info
        .pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbi_start_tvusec))
}

fn process_argv(pid: c_int) -> io::Result<Vec<OsString>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0_usize;
    // SAFETY: first sysctl call only queries required size.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            ptr::null_mut(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    let mut buffer = vec![0_u8; size];
    // SAFETY: buffer is writable for `size`; sysctl updates size to bytes written.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buffer.as_mut_ptr().cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(size);
    parse_procargs2(&buffer)
}

fn parse_procargs2(buffer: &[u8]) -> io::Result<Vec<OsString>> {
    if buffer.len() < size_of::<c_int>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short KERN_PROCARGS2",
        ));
    }
    let mut raw_argc = [0_u8; size_of::<c_int>()];
    raw_argc.copy_from_slice(&buffer[..size_of::<c_int>()]);
    let argc = c_int::from_ne_bytes(raw_argc);
    if argc <= 0 || argc > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid process argc",
        ));
    }
    let mut cursor = size_of::<c_int>();
    cursor = skip_c_string(buffer, cursor)?;
    while cursor < buffer.len() && buffer[cursor] == 0 {
        cursor += 1;
    }
    let mut argv = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        let end = buffer[cursor..]
            .iter()
            .position(|value| *value == 0)
            .map(|offset| cursor + offset)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unterminated argv"))?;
        argv.push(OsString::from_vec(buffer[cursor..end].to_vec()));
        cursor = end + 1;
    }
    // KERN_PROCARGS2 includes argv[0]; the command identity hashes executable separately.
    if !argv.is_empty() {
        argv.remove(0);
    }
    Ok(argv)
}

fn skip_c_string(buffer: &[u8], cursor: usize) -> io::Result<usize> {
    let value = CStr::from_bytes_until_nul(&buffer[cursor..])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "unterminated exec path"))?;
    Ok(cursor + value.to_bytes_with_nul().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_length_prefixed_procargs_without_environment() {
        let mut bytes = (3_i32).to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/opt/ds4\0\0/opt/ds4\0-m\0/model.gguf\0KEY=value\0");
        assert_eq!(
            parse_procargs2(&bytes).unwrap(),
            vec![OsString::from("-m"), OsString::from("/model.gguf")]
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn lists_processes_without_failing_on_transient_processes() {
        let processes = match list_processes() {
            Ok(processes) => processes,
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOENT) | Some(libc::EPERM) | Some(libc::EACCES)
                ) =>
            {
                return;
            }
            Err(error) => panic!("macOS process listing failed unexpectedly: {error}"),
        };
        assert!(
            processes
                .iter()
                .any(|process| process.pid == std::process::id())
        );
    }
}
