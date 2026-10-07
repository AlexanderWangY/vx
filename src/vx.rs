//! The core: where VMs live on disk and what's in their vx.toml.

use std::collections::HashSet;
use std::fs::{self, DirBuilder, File, TryLockError};
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::{env, io};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::hinted;
use crate::host::Arch;

const SPEC_FILE: &str = "vx.toml";
const SPEC_HEADER: &str = "# Written by `vx new`. Edit freely; changes apply on next start.\n";
const FIRST_SSH_PORT: u16 = 2222;
/// The longest socket name a backend puts in a VM directory.
const LONGEST_SOCKET: &str = "serial.sock";
/// Unix socket paths must fit in `sun_path`, minus the trailing NUL.
const MAX_SOCKET_PATH: usize = if cfg!(target_os = "macos") { 103 } else { 107 };

/// The root of everything: $VX_HOME, or ~/.vx by default.
///
/// ```text
/// images/   download cache, safe to delete
/// ssh/      vx's own SSH client key
/// vms/<n>/  one directory per VM
/// ```
pub struct Home {
    root: PathBuf,
}

impl Home {
    pub fn from_env() -> Result<Home> {
        let root = match env::var_os("VX_HOME") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => env::home_dir()
                .filter(|p| !p.as_os_str().is_empty())
                .context("can't find your home directory; set VX_HOME")?
                .join(".vx"),
        };
        let root = std::path::absolute(&root).with_context(|| format!("resolving VX_HOME {}", root.display()))?;
        Ok(Home::at(root))
    }

    pub fn at(root: impl Into<PathBuf>) -> Home {
        Home { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn images(&self) -> PathBuf {
        self.root.join("images")
    }

    pub fn ssh(&self) -> PathBuf {
        self.root.join("ssh")
    }

    pub fn vms(&self) -> PathBuf {
        self.root.join("vms")
    }

    /// Create the directory skeleton. Safe to call repeatedly.
    pub fn init(&self) -> Result<()> {
        for dir in [self.images(), self.ssh(), self.vms()] {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }

    /// Names of all VMs, sorted. Hidden directories (an in-progress `vx new`) are skipped.
    pub fn names(&self) -> Result<Vec<String>> {
        let vms = self.vms();
        let entries = match fs::read_dir(&vms) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            r => r.with_context(|| format!("reading {}", vms.display()))?,
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string()
                && !name.starts_with('.')
            {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn load(&self, name: &str) -> Result<Vm> {
        validate_name(name)?;
        let dir = self.vms().join(name);
        let path = dir.join(SPEC_FILE);
        let text = match fs::read_to_string(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && !dir.exists() => {
                return Err(hinted(format!("no VM named `{name}`"), "`vx ls` lists your VMs"));
            }
            r => r.with_context(|| format!("reading {}", path.display()))?,
        };
        let spec = Spec::from_toml(&text).with_context(|| format!("invalid {}", path.display()))?;
        Ok(Vm { name: name.into(), dir, spec })
    }

    /// Check that `name` is valid, unused, and short enough for its sockets.
    pub fn check_new_name(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let dir = self.vms().join(name);
        if dir.exists() {
            return Err(hinted(
                format!("a VM named `{name}` already exists"),
                format!("pick another name, or delete it with `vx rm {name}`"),
            ));
        }
        let len = dir.join(LONGEST_SOCKET).as_os_str().len();
        if len > MAX_SOCKET_PATH {
            return Err(hinted(
                format!(
                    "paths under {} are too long for Unix sockets ({len} > {MAX_SOCKET_PATH} bytes)",
                    dir.display()
                ),
                "use a shorter VM name, or set VX_HOME to a shorter path",
            ));
        }
        Ok(())
    }

    /// Create a VM atomically: build it in `vms/.tmp-<name>/`, then rename it into place,
    /// so a failed or interrupted `vx new` never leaves a half-made VM.
    ///
    /// `build` sees the VM at its temporary path, so it must not write `vm.dir` into files.
    pub fn create(&self, name: &str, spec: Spec, build: impl FnOnce(&Vm) -> Result<()>) -> Result<Vm> {
        self.check_new_name(name)?;
        spec.validate()?;
        self.init()?;

        let tmp = self.vms().join(format!(".tmp-{name}"));
        match DirBuilder::new().mode(0o700).create(&tmp) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(hinted(
                    format!("`{name}` is being created, or an earlier `vx new {name}` was interrupted"),
                    format!("if no other vx is running: rm -rf {}", tmp.display()),
                ));
            }
            r => r.with_context(|| format!("creating {}", tmp.display()))?,
        }

        let mut vm = Vm { name: name.into(), dir: tmp, spec };
        let dir = self.vms().join(name);
        let built = vm
            .save()
            .and_then(|()| build(&vm))
            .and_then(|()| fs::rename(&vm.dir, &dir).with_context(|| format!("moving into {}", dir.display())));
        if let Err(e) = built {
            let _ = fs::remove_dir_all(&vm.dir);
            return Err(e);
        }
        vm.dir = dir;
        Ok(vm)
    }

    /// The lowest port ≥ 2222 that no VM claims and nothing on this host is listening on.
    pub fn next_ssh_port(&self) -> Result<u16> {
        let mut claimed = HashSet::new();
        for name in self.names()? {
            // A VM with a broken vx.toml can't start, so it can't hold a port either.
            if let Ok(vm) = self.load(&name) {
                claimed.extend(vm.spec.host_ports());
            }
        }
        (FIRST_SSH_PORT..=u16::MAX)
            .find(|p| !claimed.contains(p) && TcpListener::bind((Ipv4Addr::LOCALHOST, *p)).is_ok())
            .context("no free TCP port for SSH")
    }
}

/// A VM is a directory: $VX_HOME/vms/<name>/
#[derive(Debug)]
pub struct Vm {
    pub name: String,
    pub dir: PathBuf,
    pub spec: Spec,
}

impl Vm {
    pub fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    /// Write the spec back to vx.toml. Comments added by hand are not kept.
    pub fn save(&self) -> Result<()> {
        let path = self.path(SPEC_FILE);
        let tmp = self.path(".vx.toml.tmp");
        fs::write(&tmp, self.spec.to_toml()?).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
    }

    /// Hold `.lock` while a command changes this VM. Released when the `Lock` is dropped.
    pub fn lock(&self) -> Result<Lock> {
        let path = self.path(".lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Lock(file)),
            Err(TryLockError::WouldBlock) => bail!("another vx command is working on `{}`", self.name),
            Err(TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }

    /// Remove the VM's directory. The caller stops it first.
    pub fn delete(self) -> Result<()> {
        fs::remove_dir_all(&self.dir).with_context(|| format!("deleting {}", self.dir.display()))
    }
}

/// Held while a command changes a VM; the OS releases it when the file closes.
pub struct Lock(#[allow(dead_code)] File);

/// The contents of vx.toml.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub backend: String,
    /// What the VM was created from (informational).
    pub image: String,
    pub arch: Arch,
    pub cpus: u32,
    /// Passed straight to the backend, e.g. "4G".
    pub memory: String,
    /// Extra TCP forwards on 127.0.0.1, as "host:guest".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forward: Vec<String>,
    /// Folders on this machine shared with the VM (`vx mount`).
    #[serde(default, rename = "mount", skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<Mount>,
    pub ssh: SshSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu: Option<QemuSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshSpec {
    pub user: String,
    pub port: u16,
}

/// A folder on this machine that shows up inside the VM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// An absolute path on this machine.
    pub host: PathBuf,
    /// Where it appears in the VM: absolute, or `~/…` for the VM user's home.
    pub guest: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

/// QEMU's escape hatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QemuSpec {
    /// Appended last, so they can override anything.
    #[serde(default)]
    pub args: Vec<String>,
}

impl Spec {
    /// Host-derived defaults for a new VM. `vx new` picks the real SSH port.
    pub fn defaults() -> Result<Spec> {
        Ok(Spec {
            backend: "qemu".into(),
            image: "debian-13".into(),
            arch: Arch::host()?,
            cpus: std::thread::available_parallelism().map_or(4, |n| n.get().min(4) as u32),
            memory: "4G".into(),
            forward: vec![],
            mounts: vec![],
            ssh: SshSpec { user: guest_user(&env::var("USER").unwrap_or_default()), port: FIRST_SSH_PORT },
            qemu: None,
        })
    }

    pub fn from_toml(text: &str) -> Result<Spec> {
        let spec: Spec = toml::from_str(text)?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(format!("{SPEC_HEADER}{}", toml::to_string(self)?))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.cpus >= 1, "cpus must be at least 1");
        ensure!(is_size(&self.memory), "memory `{}` should look like 4G, 512M or 4096", self.memory);
        ensure!(is_user(&self.ssh.user), "ssh.user `{}` isn't a valid Linux user name", self.ssh.user);
        ensure!(self.ssh.port != 0, "ssh.port can't be 0");
        let mut hosts = HashSet::from([self.ssh.port]);
        for f in &self.forward {
            let (host, _) = parse_forward(f)?;
            ensure!(hosts.insert(host), "host port {host} is used more than once");
        }
        let mut guests = HashSet::new();
        for m in &self.mounts {
            ensure!(m.host.is_absolute(), "mount host `{}` should be an absolute path", m.host.display());
            crate::mount::check_guest(&m.guest)?;
            ensure!(guests.insert(&m.guest), "mount guest `{}` is used more than once", m.guest);
        }
        Ok(())
    }

    /// Extra forwards as (host, guest) ports. Invalid entries are skipped; `validate` rejects them.
    pub fn forwards(&self) -> impl Iterator<Item = (u16, u16)> {
        self.forward.iter().filter_map(|f| parse_forward(f).ok())
    }

    /// Every host port this VM claims: SSH plus forwards.
    pub fn host_ports(&self) -> impl Iterator<Item = u16> {
        std::iter::once(self.ssh.port).chain(self.forwards().map(|(h, _)| h))
    }
}

/// `^[a-z][a-z0-9-]{0,31}$`, so a name works as a directory, a hostname and an ssh alias.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return Err(hinted(
            format!("`{name}` isn't a valid VM name"),
            "use up to 32 lowercase letters, digits and `-`, starting with a letter",
        ));
    }
    Ok(())
}

/// Accounts cloud images already have, or that would clash with system groups.
const TAKEN_USERS: &[&str] = &[
    "root", "daemon", "bin", "sys", "sync", "games", "man", "lp", "mail", "news", "uucp", "proxy", "www-data",
    "backup", "list", "irc", "nobody", "admin", "sshd", "debian", "ubuntu", "lxd",
];

/// Your user name, made safe for the guest: lowercased, invalid characters dropped,
/// and `dev` if nothing usable is left or the image already has that account.
fn guest_user(host_user: &str) -> String {
    let user: String = host_user
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
        .take(32)
        .collect();
    if is_user(&user) && !TAKEN_USERS.contains(&user.as_str()) { user } else { "dev".into() }
}

/// `^[a-z_][a-z0-9_-]{0,31}$`
fn is_user(s: &str) -> bool {
    s.len() <= 32
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// "8080:80" → (8080, 80)
pub fn parse_forward(s: &str) -> Result<(u16, u16)> {
    let port = |p: &str| p.parse::<u16>().ok().filter(|&p| p != 0);
    s.split_once(':')
        .and_then(|(h, g)| Some((port(h)?, port(g)?)))
        .with_context(|| format!("forward `{s}` should look like \"8080:80\" (host:guest)"))
}

/// A size like "4G", "512M" or "4096" (MiB), as QEMU and qemu-img accept it.
pub fn is_size(s: &str) -> bool {
    let digits = s.strip_suffix(['K', 'M', 'G', 'T', 'k', 'm', 'g', 't']).unwrap_or(s);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && digits.bytes().any(|b| b != b'0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hinted;
    use std::ops::Deref;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A throwaway $VX_HOME, deleted on drop.
    struct TempHome(Home);

    impl TempHome {
        fn new() -> TempHome {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            TempHome(Home::at(env::temp_dir().join(format!("vx-test-{}-{n}", std::process::id()))))
        }
    }

    impl Deref for TempHome {
        type Target = Home;
        fn deref(&self) -> &Home {
            &self.0
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.root());
        }
    }

    fn hint(e: &anyhow::Error) -> Option<&str> {
        e.chain().find_map(|c| c.downcast_ref::<Hinted>()).map(|h| h.hint.as_str())
    }

    fn spec() -> Spec {
        Spec::defaults().unwrap()
    }

    #[test]
    fn parses_hand_written_spec() {
        let spec = Spec::from_toml(
            r#"
            # Written by `vx new`. Edit freely; changes apply on next start.
            backend = "qemu"
            image   = "debian-13"
            arch    = "aarch64"
            cpus    = 4
            memory  = "4G"                 # passed straight to the backend
            forward = ["8080:80"]

            [ssh]
            user = "alexawang"
            port = 2222

            [qemu]
            args = ["-rtc", "base=localtime"]
            "#,
        )
        .unwrap();
        assert_eq!(spec.arch, Arch::Aarch64);
        assert_eq!(spec.ssh.port, 2222);
        assert_eq!(spec.host_ports().collect::<Vec<_>>(), [2222, 8080]);
        assert_eq!(spec.qemu.unwrap().args, ["-rtc", "base=localtime"]);
    }

    #[test]
    fn spec_round_trips() {
        let mut s = spec();
        s.forward = vec!["8080:80".into()];
        s.qemu = Some(QemuSpec { args: vec!["-rtc".into(), "base=localtime".into()] });
        let text = s.to_toml().unwrap();
        assert!(text.starts_with(SPEC_HEADER));
        assert_eq!(Spec::from_toml(&text).unwrap(), s);
    }

    #[test]
    fn optional_fields_are_omitted() {
        let text = spec().to_toml().unwrap();
        assert!(!text.contains("forward"), "{text}");
        assert!(!text.contains("[qemu]"), "{text}");
    }

    #[test]
    fn rejects_bad_specs() {
        // The default CPU count depends on the host, so pin it for the replacements below.
        let good = Spec { cpus: 4, ..spec() }.to_toml().unwrap();
        let bad = [
            good.replace("cpus = 4", "cpu = 4"),
            good.replace("cpus = 4", "cpus = 0"),
            good.replace("\"4G\"", "\"lots\""),
            good.replace("\"aarch64\"", "\"riscv\"").replace("\"x86_64\"", "\"riscv\""),
            format!("forward = [\"80\"]\n{good}"),
            format!("forward = [\"0:80\"]\n{good}"),
            format!("forward = [\"2222:80\"]\n{good}"),
            format!("forward = [\"8080:80\", \"8080:81\"]\n{good}"),
        ];
        for text in bad {
            assert!(Spec::from_toml(&text).is_err(), "accepted:\n{text}");
        }
    }

    #[test]
    fn guest_users() {
        assert_eq!(guest_user("alexawang"), "alexawang");
        assert_eq!(guest_user("Alexa.Wang"), "alexawang");
        assert_eq!(guest_user("root"), "dev");
        assert_eq!(guest_user("ubuntu"), "dev");
        assert_eq!(guest_user("1st"), "dev");
        assert_eq!(guest_user(""), "dev");
        assert_eq!(guest_user(&"a".repeat(40)).len(), 32);
    }

    #[test]
    fn sizes() {
        for ok in ["4G", "512M", "4096", "1t", "2k"] {
            assert!(is_size(ok), "{ok}");
        }
        for bad in ["", "G", "0", "0G", "4GB", "-4G", "4.5G", " 4G"] {
            assert!(!is_size(bad), "{bad}");
        }
    }

    #[test]
    fn names() {
        for ok in ["dev", "a", "web-1", &"a".repeat(32)] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Dev", "1dev", "-dev", "dev_1", "dev.vx", "../etc", &"a".repeat(33)] {
            let e = validate_name(bad).unwrap_err();
            assert!(hint(&e).is_some(), "{bad}");
        }
    }

    #[test]
    fn empty_home_has_no_vms() {
        let home = TempHome::new();
        assert!(home.names().unwrap().is_empty());
        assert!(!home.root().exists(), "listing must not create anything");
    }

    #[test]
    fn create_then_load() {
        let home = TempHome::new();
        let vm = home
            .create("dev", spec(), |vm| {
                assert!(vm.dir.ends_with(".tmp-dev"));
                Ok(fs::write(vm.path("disk.qcow2"), "")?)
            })
            .unwrap();
        assert_eq!(vm.dir, home.vms().join("dev"));
        assert!(vm.path("disk.qcow2").exists());
        assert!(home.images().is_dir() && home.ssh().is_dir());
        assert_eq!(home.names().unwrap(), ["dev"]);
        assert_eq!(home.load("dev").unwrap().spec, spec());
    }

    #[test]
    fn failed_create_leaves_nothing() {
        let home = TempHome::new();
        let e = home.create("dev", spec(), |_| bail!("download failed")).unwrap_err();
        assert_eq!(e.to_string(), "download failed");
        assert_eq!(fs::read_dir(home.vms()).unwrap().count(), 0);
    }

    #[test]
    fn interrupted_create_blocks_with_hint() {
        let home = TempHome::new();
        home.init().unwrap();
        fs::create_dir(home.vms().join(".tmp-dev")).unwrap();
        assert!(home.names().unwrap().is_empty());
        let e = home.create("dev", spec(), |_| Ok(())).unwrap_err();
        assert!(hint(&e).unwrap().contains("rm -rf"));
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let home = TempHome::new();
        home.create("dev", spec(), |_| Ok(())).unwrap();
        let e = home.create("dev", spec(), |_| Ok(())).unwrap_err();
        assert!(hint(&e).unwrap().contains("vx rm dev"));
    }

    #[test]
    fn missing_vm_has_hint() {
        let home = TempHome::new();
        let e = home.load("nope").unwrap_err();
        assert_eq!(e.to_string(), "no VM named `nope`");
        assert!(hint(&e).is_some());
    }

    #[test]
    fn broken_spec_names_the_file() {
        let home = TempHome::new();
        let vm = home.create("dev", spec(), |_| Ok(())).unwrap();
        fs::write(vm.path(SPEC_FILE), "cpus = \"four\"").unwrap();
        let e = home.load("dev").unwrap_err();
        assert!(format!("{e:#}").contains("dev/vx.toml"), "{e:#}");
    }

    #[test]
    fn save_persists_changes() {
        let home = TempHome::new();
        let mut vm = home.create("dev", spec(), |_| Ok(())).unwrap();
        vm.spec.ssh.port = 2299;
        vm.save().unwrap();
        assert_eq!(home.load("dev").unwrap().spec.ssh.port, 2299);
    }

    #[test]
    fn lock_is_exclusive() {
        let home = TempHome::new();
        let vm = home.create("dev", spec(), |_| Ok(())).unwrap();
        let held = vm.lock().unwrap();
        assert!(vm.lock().is_err());
        drop(held);
        assert!(vm.lock().is_ok());
    }

    #[test]
    fn delete_removes_directory() {
        let home = TempHome::new();
        let vm = home.create("dev", spec(), |_| Ok(())).unwrap();
        vm.delete().unwrap();
        assert!(home.names().unwrap().is_empty());
    }

    #[test]
    fn ssh_port_skips_claimed() {
        let home = TempHome::new();
        let mut s = spec();
        s.forward = vec![format!("{}:80", FIRST_SSH_PORT + 1)];
        home.create("dev", s, |_| Ok(())).unwrap();
        let port = home.next_ssh_port().unwrap();
        assert!(port > FIRST_SSH_PORT + 1, "{port}");
    }

    #[test]
    fn long_home_is_rejected() {
        let home = Home::at(PathBuf::from("/").join("x".repeat(100)));
        let e = home.check_new_name("dev").unwrap_err();
        assert!(hint(&e).unwrap().contains("VX_HOME"));
    }
}
