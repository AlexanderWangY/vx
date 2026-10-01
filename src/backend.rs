use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Result, bail};

use crate::qemu;
use crate::vx::Vm;

/// Asked of the backend every time, never stored.
pub enum State {
    Running,
    Paused,
    Stopped,
    /// Raw backend status we don't model, e.g. QEMU's "io-error".
    Other(String),
}

/// One line of `vx doctor` output.
pub struct Check {
    pub name: String,
    pub ok: bool,
    /// The exact command that fixes it, if any.
    pub hint: Option<String>,
}

/// What every backend must do.
pub trait Backend: Sync {
    /// Set up the backend's disk for `vm` from a cached image, resized to `disk` (e.g. "20G").
    fn create(&self, vm: &Vm, image: &Path, disk: &str) -> Result<()>;
    fn start(&self, vm: &Vm) -> Result<()>;
    fn stop(&self, vm: &Vm, force: bool) -> Result<()>;
    fn state(&self, vm: &Vm) -> State;
    /// Where sshd is reachable. QEMU: 127.0.0.1:<forwarded port>.
    fn ssh_addr(&self, vm: &Vm) -> SocketAddr;
    /// Host checks for `vx doctor`.
    fn checks(&self) -> Vec<Check> {
        vec![]
    }

    // Optional abilities. `None` means unsupported, so callers can hide them up front.
    fn pause(&self) -> Option<&dyn Pause> {
        None
    }
    fn console(&self) -> Option<&dyn Console> {
        None
    }
    fn command_line(&self) -> Option<&dyn CommandLine> {
        None
    }
}

pub trait Pause {
    fn pause(&self, vm: &Vm) -> Result<()>;
    fn resume(&self, vm: &Vm) -> Result<()>;
}

pub trait Console {
    fn attach(&self, vm: &Vm) -> Result<UnixStream>;
}

/// The exact command line a VM runs with (`vx cmd`).
pub trait CommandLine {
    fn argv(&self, vm: &Vm) -> Result<Vec<String>>;
}

/// Look up a backend by its `backend = "…"` name in vx.toml.
pub fn get(name: &str) -> Result<&'static dyn Backend> {
    match name {
        "qemu" => Ok(&qemu::Qemu),
        other => bail!("unknown backend `{other}` in vx.toml"),
    }
}
