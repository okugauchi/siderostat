use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
    net::SocketAddr,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

pub struct FakeDs4Process {
    child: Child,
    pub address: SocketAddr,
    pub startup_elapsed: Duration,
}

impl FakeDs4Process {
    pub async fn spawn(arguments: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let started = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_fake-ds4"))
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawn fake-ds4")?;
        let stdout = child.stdout.take().context("capture fake-ds4 stdout")?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .context("wait for fake-ds4 listener")??;
        let address = line
            .trim()
            .strip_prefix("fake-ds4 listening on ")
            .context("parse fake-ds4 listener line")?
            .parse()
            .context("parse fake-ds4 socket address")?;
        Ok(Self {
            child,
            address,
            startup_elapsed: started.elapsed(),
        })
    }

    pub async fn terminate(mut self) -> Result<std::process::ExitStatus> {
        let pid = self.child.id().context("fake-ds4 has no pid")?;
        send_sigterm(pid)?;
        tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .context("wait for fake-ds4 SIGTERM exit")?
            .context("wait for fake-ds4")
    }

    pub async fn wait(mut self) -> Result<std::process::ExitStatus> {
        tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .context("wait for fake-ds4 exit")?
            .context("wait for fake-ds4")
    }
}

pub fn temporary_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("siderostat-{name}-{}", uuid::Uuid::new_v4()))
}

pub async fn wait_until_file_exists(path: &Path) -> Result<()> {
    for _ in 0..50 {
        if path.is_file() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    bail!("timed out waiting for {}", path.display())
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<()> {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    const SIGTERM: i32 = 15;
    // SAFETY: `pid` comes from the owned child and SIGTERM has no pointer arguments.
    if unsafe { kill(pid as i32, SIGTERM) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("send SIGTERM to fake-ds4")
    }
}

#[cfg(not(unix))]
fn send_sigterm(_pid: u32) -> Result<()> {
    bail!("SIGTERM test support requires Unix")
}
