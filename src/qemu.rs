mod probe;

use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use crate::backend::{Backend, Check, CommandLine, Console, Pause, State};
use crate::host::{self, Arch, Os};
use crate::style::{self, ERR};
use crate::vx::Vm;
use crate::{hinted, qmp};
use probe::Host;

/// How long a guest gets to react to the power button before it's forced off.
const POWERDOWN_TIMEOUT: Duration = Duration::from_secs(60);
/// How long QEMU gets to exit after `quit` or SIGKILL.
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Qemu;

impl Backend for Qemu {
    fn create(&self, vm: &Vm, image: &Path, disk: &str) -> Result<()> {
        let qemu_img =
            host::which("qemu-img").ok_or_else(|| hinted("qemu-img not found", probe::install_hint(Os::host())))?;
        let target = vm.path("disk.qcow2");
        let info = host::run(Command::new(&qemu_img).args(["info", "--output=json"]).arg(image))?;
        let info: Value = serde_json::from_str(&info).context("reading qemu-img info")?;
        if info["format"] == "qcow2" {
            // A copy-on-write clone on APFS, btrfs and XFS, so it's instant and takes no space.
            fs::copy(image, &target).with_context(|| format!("copying {}", image.display()))?;
        } else {
            host::run(Command::new(&qemu_img).args(["convert", "-O", "qcow2"]).arg(image).arg(&target))?;
        }
        // The guest's cloud-init grows its root filesystem to fill the disk on first boot.
        host::run(Command::new(&qemu_img).args(["resize", "-q"]).arg(&target).arg(disk))?;
        Ok(())
    }

    fn start(&self, vm: &Vm) -> Result<()> {
        let host = Host::probe(vm.spec.arch)?;
        match (&host.accel, vm.spec.arch == host.arch) {
            (Ok(_), true) => {}
            (Err(why), true) => {
                style::warn("", why);
                style::warn("", format!("{} will run emulated, which is much slower", vm.name));
            }
            (_, false) => style::warn(
                "",
                format!(
                    "{} is an {} VM on an {} host, so it will run emulated, which is much slower",
                    vm.name, vm.spec.arch, host.arch
                ),
            ),
        }

        // With -daemonize, QEMU exits once the VM is set up and its sockets are listening.
        let out = Command::new(&host.qemu)
            .args(argv(vm, &host))
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", host.qemu.display()))?;
        if out.status.success() {
            return Ok(());
        }

        let log_path = vm.path("qemu.log");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let log = fs::read_to_string(&log_path).unwrap_or_default();
        let why = last_line(&stderr).or_else(|| last_line(&log)).unwrap_or("no error output");
        let hint = match start_hint(&format!("{stderr}\n{log}")) {
            Some(hint) => hint.to_string(),
            None => format!("QEMU's log is in {}", log_path.display()),
        };
        Err(hinted(format!("QEMU could not start {}: {why}", vm.name), hint))
    }

    fn stop(&self, vm: &Vm, force: bool) -> Result<()> {
        let sock = vm.path("qmp.sock");
        // A paused guest can't react to the power button, so it goes straight to `quit`.
        if !force && self.state(vm) == State::Running {
            qmp::call(&sock, "system_powerdown")?;
            eprintln!("{}", ERR.dim(format!("waiting for {} to shut down…", vm.name)));
            if self.wait_until_stopped(vm, POWERDOWN_TIMEOUT) {
                return Ok(());
            }
            style::warn("", format!("{} ignored the shutdown request; forcing it off", vm.name));
        }

        let _ = qmp::call(&sock, "quit"); // QEMU may exit before it replies
        if self.wait_until_stopped(vm, EXIT_TIMEOUT) {
            return Ok(());
        }

        // QMP still answers, so the pidfile belongs to a live QEMU.
        let pid = read_pid(vm)?;
        // SAFETY: kill(2) has no memory-safety preconditions.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            return Err(io::Error::last_os_error()).with_context(|| format!("killing QEMU (pid {pid})"));
        }
        ensure!(self.wait_until_stopped(vm, EXIT_TIMEOUT), "{} is still running after SIGKILL", vm.name);
        Ok(())
    }

    fn state(&self, vm: &Vm) -> State {
        state_from(qmp::call(&vm.path("qmp.sock"), "query-status"))
    }

    fn ssh_addr(&self, vm: &Vm) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, vm.spec.ssh.port))
    }

    fn disk_usage(&self, vm: &Vm) -> Option<u64> {
        // Allocated blocks rather than the length, which is sparse or a copy-on-write clone.
        fs::metadata(vm.path("disk.qcow2")).ok().map(|m| m.blocks() * 512)
    }

    fn checks(&self) -> Vec<Check> {
        let hint = Some(probe::install_hint(Os::host()).to_string());
        let qemu = Arch::host().and_then(Host::probe);
        vec![
            Check { name: "QEMU".into(), ok: qemu.is_ok(), hint: hint.clone() },
            Check { name: "qemu-img".into(), ok: host::which("qemu-img").is_some(), hint },
        ]
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

impl Qemu {
    fn wait_until_stopped(&self, vm: &Vm, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.state(vm) == State::Stopped {
                return true;
            }
            thread::sleep(Duration::from_millis(200));
        }
        false
    }
}

impl Pause for Qemu {
    fn pause(&self, vm: &Vm) -> Result<()> {
        qmp::call(&vm.path("qmp.sock"), "stop").map(drop)
    }

    fn resume(&self, vm: &Vm) -> Result<()> {
        qmp::call(&vm.path("qmp.sock"), "cont").map(drop)
    }
}

impl Console for Qemu {
    fn attach(&self, vm: &Vm) -> Result<UnixStream> {
        let sock = vm.path("serial.sock");
        UnixStream::connect(&sock).with_context(|| format!("connecting to {}", sock.display()))
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

/// Map a `query-status` reply, or the reason there wasn't one, to a `State`.
fn state_from(reply: Result<Value>) -> State {
    match reply {
        Ok(r) => match r["status"].as_str() {
            Some("running") => State::Running,
            Some("paused") => State::Paused,
            Some(other) => State::Other(other.into()), // e.g. "io-error": the host disk is full
            None => State::Other("unknown".into()),
        },
        Err(e) => match e.chain().find_map(|c| c.downcast_ref::<io::Error>()).map(io::Error::kind) {
            // No socket, or a stale one left behind by a crash.
            Some(io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => State::Stopped,
            // Connected but no greeting: another client holds QEMU's only QMP slot.
            Some(io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => State::Running,
            _ => State::Other(format!("unknown ({e})")),
        },
    }
}

/// Known QEMU startup errors and how to fix them.
fn start_hint(output: &str) -> Option<&'static str> {
    const HINTS: &[(&str, &str)] = &[
        (
            "Could not set up host forwarding rule",
            "another program is using one of this VM's ports; change [ssh] port or forward in vx.toml",
        ),
        ("Failed to get \"write\" lock", "another QEMU is already using this VM's disk"),
        ("com.apple.security.hypervisor", "QEMU isn't signed for HVF; run `brew reinstall qemu`"),
        ("HV_DENIED", "QEMU isn't signed for HVF; run `brew reinstall qemu`"),
        (
            "Could not find ROM image",
            "aarch64 UEFI firmware is missing; install qemu-efi-aarch64 (Debian, Ubuntu) or edk2-aarch64 (Fedora)",
        ),
    ];
    HINTS.iter().find(|(needle, _)| output.contains(needle)).map(|(_, hint)| *hint)
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|l| !l.is_empty())
}

fn read_pid(vm: &Vm) -> Result<i32> {
    let path = vm.path("qemu.pid");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    text.trim().parse().with_context(|| format!("no pid in {}", path.display()))
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

    #[test]
    fn state_from_replies() {
        let status = |s: &str| state_from(Ok(serde_json::json!({ "status": s, "running": s == "running" })));
        assert_eq!(status("running"), State::Running);
        assert_eq!(status("paused"), State::Paused);
        assert_eq!(status("io-error"), State::Other("io-error".into()));
    }

    #[test]
    fn state_from_connection_errors() {
        let err = |kind: io::ErrorKind| state_from(Err(io::Error::from(kind).into()));
        assert_eq!(err(io::ErrorKind::NotFound), State::Stopped);
        assert_eq!(err(io::ErrorKind::ConnectionRefused), State::Stopped);
        assert_eq!(err(io::ErrorKind::WouldBlock), State::Running);
        assert!(matches!(err(io::ErrorKind::PermissionDenied), State::Other(_)));
    }

    #[test]
    fn start_hints() {
        let port = "qemu-system-aarch64: -netdev user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22: \
                    Could not set up host forwarding rule 'tcp:127.0.0.1:2222-:22'";
        assert!(start_hint(port).unwrap().contains("[ssh] port"));
        assert!(start_hint("Could not find ROM image 'edk2-aarch64-code.fd'").unwrap().contains("firmware"));
        assert_eq!(start_hint("something new"), None);
    }
}
