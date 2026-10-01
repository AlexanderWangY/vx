mod probe;

use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::Result;

use crate::backend::{Backend, CommandLine, Console, Pause, State};
use crate::host::{Arch, Os};
use crate::vx::Vm;
use probe::Host;

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
    fn argv(&self, vm: &Vm) -> Result<Vec<String>> {
        let host = Host::probe(vm.spec.arch)?;
        let mut a = vec![host.qemu.display().to_string()];
        a.extend(argv(vm, &host));
        Ok(a)
    }
}

/// Every QEMU argument for `vm` on `host`, after the binary. Pure, so it's golden-tested,
/// and `vx cmd` prints it verbatim: nothing about how a VM runs is hidden.
pub fn argv(vm: &Vm, host: &Host) -> Vec<String> {
    let s = &vm.spec;
    let p = |file: &str| qemu_path(&vm.path(file));
    let native = s.arch == host.arch && host.accel.is_ok();

    let mut net = format!("user,id=net0,hostfwd=tcp:127.0.0.1:{}-:22", s.ssh.port);
    for (h, g) in s.forwards() {
        net += &format!(",hostfwd=tcp:127.0.0.1:{h}-:{g}");
    }
    let machine = match s.arch {
        Arch::Aarch64 => "virt,gic-version=max",
        Arch::X86_64 => "q35",
    };
    let accel = match &host.accel {
        Ok(accel) if native => accel,
        _ => "tcg",
    };
    let cpu = match (native, host.os, s.arch) {
        // HVF on Intel Macs aborts on these features (Lima's fix).
        (true, Os::Macos, Arch::X86_64) => "host,-pdpe1gb,-avx512vl",
        _ => "max",
    };

    let mut a = Vec::new();
    let mut opt = |k: &str, v: String| a.extend([k.to_string(), v]);
    opt("-name", vm.name.clone());
    opt("-machine", machine.into());
    opt("-accel", accel.into());
    opt("-cpu", cpu.into());
    opt("-smp", s.cpus.to_string());
    opt("-m", s.memory.clone());
    opt("-drive", format!("if=virtio,format=qcow2,discard=unmap,file={}", p("disk.qcow2")));
    opt("-drive", format!("if=virtio,format=raw,readonly=on,file={}", p("seed.img")));
    opt("-smbios", "type=1,serial=ds=nocloud".into()); // makes cloud-init look for the seed
    opt("-netdev", net);
    opt("-device", "virtio-net-pci,netdev=net0".into());
    opt("-device", "virtio-rng-pci".into()); // fast entropy on first boot
    opt("-chardev", format!("socket,id=con,path={},server=on,wait=off,logfile={}", p("serial.sock"), p("serial.log")));
    opt("-serial", "chardev:con".into());
    opt("-qmp", format!("unix:{},server=on,wait=off", p("qmp.sock")));
    opt("-pidfile", p("qemu.pid"));
    opt("-D", p("qemu.log")); // startup and runtime errors
    opt("-display", "none".into()); // never -nographic or a window with -daemonize
    if s.arch == Arch::Aarch64
        && let Some(fw) = &host.firmware
    {
        opt("-bios", fw.clone());
    }
    a.extend(["-nodefaults", "-no-user-config", "-daemonize"].map(String::from));
    if let Some(q) = &s.qemu {
        a.extend(q.args.iter().cloned()); // the escape hatch goes last, so it wins
    }
    a
}

/// QEMU splits option values on `,`, so a literal comma is written `,,`.
fn qemu_path(path: &Path) -> String {
    path.display().to_string().replace(',', ",,")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vx::{QemuSpec, Spec, SshSpec};

    fn vm(arch: Arch) -> Vm {
        Vm {
            name: "dev".into(),
            dir: "/Users/me/.vx/vms/dev".into(),
            spec: Spec {
                backend: "qemu".into(),
                image: "debian-13".into(),
                arch,
                cpus: 4,
                memory: "4G".into(),
                forward: vec![],
                ssh: SshSpec { user: "me".into(), port: 2222 },
                qemu: None,
            },
        }
    }

    fn host(os: Os, arch: Arch, accel: Result<&'static str, String>) -> Host {
        Host {
            os,
            arch,
            qemu: format!("/usr/bin/qemu-system-{arch}").into(),
            accel,
            firmware: Some("/opt/homebrew/share/qemu/edk2-aarch64-code.fd".into()),
        }
    }

    /// One option per line, so a failing test shows a readable diff.
    fn render(argv: &[String]) -> String {
        let mut out = String::new();
        for arg in argv {
            if arg.starts_with('-') && !out.is_empty() {
                out.push('\n');
            } else if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(arg);
        }
        out
    }

    #[test]
    fn macos_aarch64_hvf() {
        let mut vm = vm(Arch::Aarch64);
        vm.spec.forward = vec!["8080:80".into()];
        vm.spec.qemu = Some(QemuSpec { args: vec!["-rtc".into(), "base=localtime".into()] });
        let got = render(&argv(&vm, &host(Os::Macos, Arch::Aarch64, Ok("hvf"))));
        assert_eq!(
            got,
            "\
-name dev
-machine virt,gic-version=max
-accel hvf
-cpu max
-smp 4
-m 4G
-drive if=virtio,format=qcow2,discard=unmap,file=/Users/me/.vx/vms/dev/disk.qcow2
-drive if=virtio,format=raw,readonly=on,file=/Users/me/.vx/vms/dev/seed.img
-smbios type=1,serial=ds=nocloud
-netdev user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22,hostfwd=tcp:127.0.0.1:8080-:80
-device virtio-net-pci,netdev=net0
-device virtio-rng-pci
-chardev socket,id=con,path=/Users/me/.vx/vms/dev/serial.sock,server=on,wait=off,logfile=/Users/me/.vx/vms/dev/serial.log
-serial chardev:con
-qmp unix:/Users/me/.vx/vms/dev/qmp.sock,server=on,wait=off
-pidfile /Users/me/.vx/vms/dev/qemu.pid
-D /Users/me/.vx/vms/dev/qemu.log
-display none
-bios /opt/homebrew/share/qemu/edk2-aarch64-code.fd
-nodefaults
-no-user-config
-daemonize
-rtc base=localtime"
        );
    }

    #[test]
    fn linux_x86_64_kvm() {
        let mut host = host(Os::Linux, Arch::X86_64, Ok("kvm"));
        host.firmware = None;
        let got = render(&argv(&vm(Arch::X86_64), &host));
        assert_eq!(
            got,
            "\
-name dev
-machine q35
-accel kvm
-cpu max
-smp 4
-m 4G
-drive if=virtio,format=qcow2,discard=unmap,file=/Users/me/.vx/vms/dev/disk.qcow2
-drive if=virtio,format=raw,readonly=on,file=/Users/me/.vx/vms/dev/seed.img
-smbios type=1,serial=ds=nocloud
-netdev user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22
-device virtio-net-pci,netdev=net0
-device virtio-rng-pci
-chardev socket,id=con,path=/Users/me/.vx/vms/dev/serial.sock,server=on,wait=off,logfile=/Users/me/.vx/vms/dev/serial.log
-serial chardev:con
-qmp unix:/Users/me/.vx/vms/dev/qmp.sock,server=on,wait=off
-pidfile /Users/me/.vx/vms/dev/qemu.pid
-D /Users/me/.vx/vms/dev/qemu.log
-display none
-nodefaults
-no-user-config
-daemonize"
        );
    }

    /// Picks out the value after `flag`.
    fn value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        argv.iter().position(|a| a == flag).map(|i| argv[i + 1].as_str())
    }

    #[test]
    fn other_arch_is_emulated() {
        let argv = argv(&vm(Arch::X86_64), &host(Os::Macos, Arch::Aarch64, Ok("hvf")));
        assert_eq!(value(&argv, "-accel"), Some("tcg"));
        assert_eq!(value(&argv, "-cpu"), Some("max"));
        assert_eq!(value(&argv, "-machine"), Some("q35"));
        assert_eq!(value(&argv, "-bios"), None);
    }

    #[test]
    fn no_accelerator_is_emulated() {
        let argv = argv(&vm(Arch::Aarch64), &host(Os::Linux, Arch::Aarch64, Err("no kvm".into())));
        assert_eq!(value(&argv, "-accel"), Some("tcg"));
        assert!(value(&argv, "-bios").is_some());
    }

    #[test]
    fn intel_mac_cpu_workaround() {
        let argv = argv(&vm(Arch::X86_64), &host(Os::Macos, Arch::X86_64, Ok("hvf")));
        assert_eq!(value(&argv, "-cpu"), Some("host,-pdpe1gb,-avx512vl"));
    }

    #[test]
    fn commas_in_paths_are_escaped() {
        let mut vm = vm(Arch::Aarch64);
        vm.dir = "/Users/a,b/.vx/vms/dev".into();
        let argv = argv(&vm, &host(Os::Macos, Arch::Aarch64, Ok("hvf")));
        assert_eq!(value(&argv, "-pidfile"), Some("/Users/a,,b/.vx/vms/dev/qemu.pid"));
    }
}
