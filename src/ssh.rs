//! SSH: vx's own client key, a pinned host key per VM, the ssh_config shared by `vx ssh` and
//! plain `ssh <name>.vx`, and waiting until a booting VM accepts logins.

use std::fs;
use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::backend::{Backend, Check, State};
use crate::hinted;
use crate::host::{self, Os};
use crate::progress::{self, Spinner};
use crate::style::{self, ERR};
use crate::vx::{Home, Vm};

pub struct HostKey {
    pub private: String,
    pub public: String,
}

/// The name a VM goes by in ssh_config and known_hosts.
pub fn alias(name: &str) -> String {
    format!("{name}.vx")
}

pub fn checks() -> Vec<Check> {
    let hint = match Os::host() {
        Os::Macos => "OpenSSH ships with macOS; check your PATH",
        Os::Linux => "install openssh-client (Debian, Ubuntu) or openssh-clients (Fedora)",
    };
    ["ssh", "ssh-keygen"]
        .into_iter()
        .map(|tool| Check { name: tool.into(), ok: host::which(tool).is_some(), hint: Some(hint.into()) })
        .collect()
}

/// vx's client key, created on first use. Returns the public key line.
pub fn client_key(home: &Home) -> Result<String> {
    let key = home.ssh().join("id_ed25519");
    if !key.exists() {
        home.init()?;
        keygen(&key, "vx")?;
    }
    let public = key.with_extension("pub");
    fs::read_to_string(&public).with_context(|| format!("reading {}", public.display()))
}

/// Generate the VM's sshd host key and pin it in known_hosts, so even the first connection
/// is verified.
pub fn host_key(vm: &Vm) -> Result<HostKey> {
    let key = vm.path("host_key");
    keygen(&key, &alias(&vm.name))?;
    let private = fs::read_to_string(&key)?;
    let public = fs::read_to_string(key.with_extension("pub"))?;
    let mut fields = public.split_whitespace();
    let (Some(kind), Some(base64)) = (fields.next(), fields.next()) else {
        bail!("unexpected ssh-keygen output in {}.pub", key.display());
    };
    fs::write(vm.path("known_hosts"), format!("{} {kind} {base64}\n", alias(&vm.name)))?;
    Ok(HostKey { private, public })
}

fn keygen(path: &Path, comment: &str) -> Result<()> {
    host::run(Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"]).arg(path))?;
    Ok(())
}

/// Write the VM's ssh_config and return its path. Rewritten before every use, since it holds
/// absolute paths and the port can change.
pub fn write_config(home: &Home, vm: &Vm, addr: SocketAddr) -> Result<PathBuf> {
    let alias = alias(&vm.name);
    let text = format!(
        "Host {alias}\n  \
           HostName {ip}\n  \
           Port {port}\n  \
           User {user}\n  \
           IdentityFile \"{key}\"\n  \
           IdentitiesOnly yes\n  \
           HostKeyAlias {alias}\n  \
           UserKnownHostsFile \"{known}\"\n  \
           StrictHostKeyChecking yes\n  \
           LogLevel ERROR\n",
        ip = addr.ip(),
        port = addr.port(),
        user = vm.spec.ssh.user,
        key = home.ssh().join("id_ed25519").display(),
        known = vm.path("known_hosts").display(),
    );
    let path = vm.path("ssh_config");
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// `ssh` into the VM, through its ssh_config.
pub fn command(config: &Path, vm: &Vm) -> Command {
    ssh(config, vm)
}

fn ssh(config: &Path, vm: &Vm) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-F").arg(config).arg(alias(&vm.name));
    cmd
}

/// Replace this process with an SSH session into the VM.
pub fn exec(config: &Path, vm: &Vm, command: &[String]) -> Result<()> {
    let err = ssh(config, vm).args(command).exec();
    Err(err).context("running ssh")
}

/// Whether the VM accepts an SSH login right now.
pub fn reachable(config: &Path, vm: &Vm) -> bool {
    ssh(config, vm)
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=3", "-o", "ServerAliveInterval=2", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Replace this process with `scp`, which copies `operands` (sources, then a destination) using
/// the VM's ssh_config. `-r` lets any of them be a directory.
pub fn copy(config: &Path, operands: &[String]) -> Result<()> {
    let err = Command::new("scp").arg("-F").arg(config).arg("-r").args(operands).exec();
    Err(err).context("running scp")
}

/// Wait until the VM accepts an SSH login, showing its latest boot output meanwhile.
pub fn wait_ready(config: &Path, vm: &Vm, backend: &dyn Backend, timeout: Duration) -> Result<()> {
    let mut spinner = Spinner::new();
    let deadline = Instant::now() + timeout;
    loop {
        let probe = ssh(config, vm)
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("running ssh")?;
        if spin(probe, &mut spinner, "booting…", || last_boot_line(vm))?.success() {
            spinner.clear();
            return Ok(());
        }
        if backend.state(vm) == State::Stopped {
            spinner.clear();
            return Err(hinted(format!("{} stopped while booting", vm.name), format!("vx logs {}", vm.name)));
        }
        if Instant::now() > deadline {
            spinner.clear();
            eprintln!("  last boot output:");
            for line in boot_tail(vm, 20) {
                eprintln!("    {}", ERR.dim(line));
            }
            return Err(hinted(
                format!("{} didn't accept SSH logins within {} s", vm.name, timeout.as_secs()),
                format!("watch it boot with `vx console {}`", vm.name),
            ));
        }
        let pause = Instant::now() + Duration::from_secs(1);
        while Instant::now() < pause {
            spinner.update("booting…", &last_boot_line(vm));
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// On first boot, wait for cloud-init to finish setting the VM up.
pub fn wait_cloud_init(config: &Path, vm: &Vm) -> Result<()> {
    let mut spinner = Spinner::new();
    let child = ssh(config, vm)
        .args(["-o", "BatchMode=yes", "cloud-init", "status", "--wait"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("running ssh")?;
    let status = spin(child, &mut spinner, "finishing cloud-init…", String::new)?;
    spinner.clear();
    match status.code() {
        Some(0) => return Ok(()),
        // Done, with recoverable errors.
        Some(2) => {
            style::warn(
                "  ",
                format!("cloud-init finished with warnings; see `vx ssh {} -- cloud-init status --long`", vm.name),
            );
            return Ok(());
        }
        _ => {}
    }
    // Some images (Fedora, for one) try to set the hostname before D-Bus is up. That fails
    // the whole run, but everything else, including users and keys, is in place.
    let report = ssh(config, vm)
        .args(["-o", "BatchMode=yes", "cloud-init", "status", "--format", "json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("running ssh")?;
    if only_hostname_failed(&String::from_utf8_lossy(&report.stdout)) {
        style::warn("  ", "cloud-init couldn't set the hostname, so it's still the image's default");
        return Ok(());
    }
    Err(hinted(format!("cloud-init failed in {}", vm.name), format!("vx ssh {} -- cloud-init status --long", vm.name)))
}

/// Whether `cloud-init status --format json` lists errors, all from the set_hostname module.
fn only_hostname_failed(json: &str) -> bool {
    let Ok(status) = serde_json::from_str::<serde_json::Value>(json) else {
        return false;
    };
    let Some(errors) = status["errors"].as_array() else {
        return false;
    };
    !errors.is_empty() && errors.iter().all(|e| e.as_str().is_some_and(|e| e.starts_with("('set_hostname',")))
}

/// Animate the spinner until `child` exits.
fn spin(mut child: Child, spinner: &mut Spinner, msg: &str, detail: impl Fn() -> String) -> Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        spinner.update(msg, &detail());
        thread::sleep(Duration::from_millis(100));
    }
}

fn last_boot_line(vm: &Vm) -> String {
    boot_tail(vm, 1).pop().unwrap_or_default()
}

/// The last `n` non-empty lines of the serial log, as plain text.
fn boot_tail(vm: &Vm, n: usize) -> Vec<String> {
    let log = fs::read(vm.path("serial.log")).unwrap_or_default();
    let tail = String::from_utf8_lossy(&log[log.len().saturating_sub(8192)..]).into_owned();
    let lines: Vec<String> =
        tail.split(['\r', '\n']).map(|l| progress::plain(l).trim().to_string()).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vx::Spec;

    #[test]
    fn hostname_only_failures() {
        let hostname = r#"('set_hostname', SetHostnameError(\"Failed to set the hostname to dev (dev): ...\"))"#;
        let other = r#"('users_groups', ValueError(\"bad user\"))"#;
        let status = |errors: &[&str]| serde_json::json!({ "status": "error", "errors": errors }).to_string();
        assert!(only_hostname_failed(&status(&[hostname, hostname])));
        assert!(!only_hostname_failed(&status(&[hostname, other])));
        assert!(!only_hostname_failed(&status(&[])));
        assert!(!only_hostname_failed("usage: cloud-init status [-h] [-l] [-w]"));
    }

    #[test]
    fn config_pins_the_host_key() {
        let home = Home::at("/Users/me/.vx");
        let vm = Vm { name: "dev".into(), dir: "/Users/me/.vx/vms/dev".into(), spec: Spec::defaults().unwrap() };
        let dir = std::env::temp_dir().join(format!("vx-ssh-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let vm = Vm { dir: dir.clone(), ..vm };
        let path = write_config(&home, &vm, "127.0.0.1:2222".parse().unwrap()).unwrap();
        let text = fs::read_to_string(path).unwrap();
        fs::remove_dir_all(dir).unwrap();
        assert!(text.starts_with("Host dev.vx\n  HostName 127.0.0.1\n  Port 2222\n"), "{text}");
        assert!(text.contains("  IdentityFile \"/Users/me/.vx/ssh/id_ed25519\"\n"));
        assert!(text.contains("  HostKeyAlias dev.vx\n"));
        assert!(text.contains("  StrictHostKeyChecking yes\n"));
    }
}
