use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::Result;

use crate::backend::{Backend, CommandLine, Console, Pause, State};
use crate::vx::Vm;

pub struct Qemu;

impl Backend for Qemu {
    fn create(&self, _vm: &Vm, _image: &Path, _disk: &str) -> Result<()> {
        todo!("copy image to disk.qcow2, qemu-img resize")
    }

    fn start(&self, _vm: &Vm) -> Result<()> {
        todo!("spawn qemu-system-* -daemonize")
    }

    fn stop(&self, _vm: &Vm, _force: bool) -> Result<()> {
        todo!("system_powerdown -> quit -> SIGKILL")
    }

    fn state(&self, _vm: &Vm) -> State {
        todo!("query-status over QMP")
    }

    fn ssh_addr(&self, _vm: &Vm) -> SocketAddr {
        todo!("127.0.0.1:<ssh port from vx.toml>")
    }

    fn pause(&self) -> Option<&dyn Pause> {
        Some(self)
    }

    fn console(&self) -> Option<&dyn Console> {
        Some(self)
    }

    fn command_line(&self) -> Option<&dyn CommandLine> {
        Some(self)
    }
}

impl Pause for Qemu {
    fn pause(&self, _vm: &Vm) -> Result<()> {
        todo!("QMP stop")
    }

    fn resume(&self, _vm: &Vm) -> Result<()> {
        todo!("QMP cont")
    }
}

impl Console for Qemu {
    fn attach(&self, _vm: &Vm) -> Result<UnixStream> {
        todo!("connect to serial.sock")
    }
}

impl CommandLine for Qemu {
    fn argv(&self, _vm: &Vm) -> Result<Vec<String>> {
        todo!("build the qemu-system-* argv")
    }
}
