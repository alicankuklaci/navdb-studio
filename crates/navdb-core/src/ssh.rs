//! SSH tunnelling through the system `ssh` client (`ssh -N -L`).
//!
//! Key-based and agent authentication are supported; interactive password
//! prompts are not, because the tunnel runs non-interactively (BatchMode).
//! Windows 10+ ships the same OpenSSH client (`ssh.exe`) as an optional feature.

use anyhow::{bail, Context, Result};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::SshConfig;

/// False in builds without the `ssh-tunnel` feature (the sandboxed App Store build).
pub const AVAILABLE: bool = cfg!(feature = "ssh-tunnel");

pub struct Tunnel {
    child: Child,
    pub local_port: u16,
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

impl Tunnel {
    pub fn open(ssh: &SshConfig, remote_host: &str, remote_port: u16) -> Result<Tunnel> {
        if ssh.host.is_empty() {
            bail!("SSH host is empty");
        }
        let local_port = free_port()?;
        let mut cmd = Command::new("ssh");
        cmd.arg("-N")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg("ServerAliveInterval=30")
            .arg("-L")
            .arg(format!("127.0.0.1:{}:{}:{}", local_port, remote_host, remote_port))
            .arg("-p")
            .arg(if ssh.port == 0 { "22".to_string() } else { ssh.port.to_string() });
        if !ssh.private_key.is_empty() {
            cmd.arg("-i").arg(&ssh.private_key);
        }
        let target = if ssh.user.is_empty() { ssh.host.clone() } else { format!("{}@{}", ssh.user, ssh.host) };
        cmd.arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            // the GUI build has no console; keep ssh.exe from opening its own window
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().context("starting ssh (is OpenSSH installed?)")?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if TcpStream::connect_timeout(&format!("127.0.0.1:{}", local_port).parse()?, Duration::from_millis(200)).is_ok() {
                return Ok(Tunnel { child, local_port });
            }
            if let Some(status) = child.try_wait()? {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut err);
                }
                bail!("ssh tunnel exited ({}): {}", status, err.trim());
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                bail!("timed out establishing SSH tunnel");
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
