use std::fmt;
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Result, bail};

use crate::qemu;
use crate::vx::Vm;

/// Asked of the backend every time, never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Running,
    Paused,
    Stopped,
    /// Raw backend status we don't model, e.g. QEMU's "io-error".
    Other(String),
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            State::Running => "running",
            State::Paused => "paused",
            State::Stopped => "stopped",
            State::Other(s) => s,
        })
    }
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
    /// Bytes the VM's disk takes up on the host, which grows as the guest writes.
    fn disk_usage(&self, _vm: &Vm) -> Option<u64> {
        None
    }
    /// What a running VM has right now, as (CPUs, bytes of memory), which can differ from
    /// vx.toml until it restarts.
    fn running_size(&self, _vm: &Vm) -> Option<(u32, u64)> {
        None
    }
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
    fn snapshots(&self) -> Option<&dyn Snapshots> {
        None
    }
    fn forwards(&self) -> Option<&dyn Forwards> {
        None
    }
    fn clones(&self) -> Option<&dyn Clones> {
        None
    }
    fn disks(&self) -> Option<&dyn Disks> {
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

/// A snapshot as the backend keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snap {
    pub name: String,
    /// Seconds since the Unix epoch.
    pub created: u64,
    /// Bytes of saved memory; 0 for a snapshot of the disk alone.
    pub memory: u64,
}

pub trait Snapshots {
    /// Oldest first.
    fn list(&self, vm: &Vm) -> Result<Vec<Snap>>;
    /// Save the VM as it is now. A running VM keeps running. With `memory`, its memory is
    /// saved too if the backend can. Returns the snapshot.
    fn save(&self, vm: &Vm, name: &str, memory: bool) -> Result<Snap>;
    /// Put the VM back as it was at `snap`: running from that moment if its memory was saved,
    /// otherwise its disk as it was, and running again (from boot) if it was running.
    fn restore(&self, vm: &Vm, snap: &Snap) -> Result<()>;
    fn delete(&self, vm: &Vm, name: &str) -> Result<()>;
}

/// Changing a running VM's port forwards. vx.toml holds them across restarts; this is only
/// what the VM does right now.
pub trait Forwards {
    /// Forward TCP 127.0.0.1:`host` on this machine to `guest` in the VM.
    fn add(&self, vm: &Vm, host: u16, guest: u16) -> Result<()>;
    fn remove(&self, vm: &Vm, host: u16) -> Result<()>;
    /// What the running VM forwards, as (host, guest), SSH included.
    fn active(&self, vm: &Vm) -> Result<Vec<(u16, u16)>>;
}

/// Copying a VM's disk for `vx clone`.
pub trait Clones {
    /// Make `to`'s disk a copy of `from`'s, as it is now or, with `snap`, as it was at that
    /// snapshot. The copy has none of `from`'s snapshots. A running `from` keeps running.
    fn clone_disk(&self, from: &Vm, snap: Option<&str>, to: &Vm) -> Result<()>;
}

/// Growing a VM's disk (`vx set --disk`).
pub trait Disks {
    /// How big the disk is, as the VM sees it, in bytes.
    fn disk_size(&self, vm: &Vm) -> Result<u64>;
    /// Make the disk `bytes` big. It only grows; a running VM sees the new size straight away.
    fn grow_disk(&self, vm: &Vm, bytes: u64) -> Result<()>;
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
