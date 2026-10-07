//! `vx mount`: share folders on this machine with a VM. vx.toml's `[[mount]]` list holds them,
//! so they come back each time the VM starts, and a running VM picks up changes straight away.
//!
//! It works the way Lima's reverse-sshfs does, so it needs nothing from the guest kernel and
//! nothing listening on this machine: the VM runs `sshfs -o passive` over an ordinary `vx ssh`
//! connection, and this machine's own `sftp-server` answers it through that connection's
//! stdin and stdout. The only thing the VM needs is the sshfs package, which is installed the
//! first time it's missing.
//!
//! While a VM with mounts runs, a small background process of vx's own (`vx mount-agent`) keeps
//! them up: it waits for the VM to boot, mounts each folder, puts back any that drop, follows
//! changes to vx.toml, and leaves once the VM stops. It writes how each mount is doing to
//! `mounts.json` and what went wrong to `mounts.log`, both in the VM's directory.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::backend::{self, State};
use crate::progress::Spinner;
use crate::style::{self, OUT};
use crate::vx::{Home, Mount, Vm};
use crate::{hinted, setup, ssh};

const STATUS: &str = "mounts.json";
const LOG: &str = "mounts.log";
/// Held by the agent for as long as it runs.
const AGENT_LOCK: &str = "mounts.lock";
/// What the guest side exits with when sshfs isn't installed.
const NO_SSHFS: i32 = 3;
/// How often the agent asks the backend whether the VM is still up.
const STATE_EVERY: Duration = Duration::from_secs(3);
/// How long a mount that failed waits before it's tried again.
const RETRY_AFTER: Duration = Duration::from_secs(10);

/// Places in the VM that a mount would break, or hide what the VM needs to run.
const RESERVED: &[&str] = &[
    "/", "/bin", "/boot", "/dev", "/etc", "/home", "/lib", "/lib64", "/opt", "/proc", "/root", "/run", "/sbin", "/srv",
    "/sys", "/tmp", "/usr", "/var", "~", "~/.ssh",
];

/// `~/code` shares that folder at the same place in the VM's home; `/data:/srv/data` puts it
/// somewhere else. A folder outside your home goes to the same path in the VM.
pub fn parse(arg: &str, read_only: bool) -> Result<Mount> {
    let (host, guest) = match arg.split_once(':') {
        Some((host, guest)) => (host, Some(guest)),
        None => (arg, None),
    };
    let host = resolve(host)?;
    let guest = match guest {
        Some(guest) => normalize(guest),
        None => default_guest(&host, std::env::home_dir().as_deref())?,
    };
    check_guest(&guest)?;
    Ok(Mount { host, guest, read_only })
}

/// The absolute, symlink-free path of a folder on this machine.
fn resolve(host: &str) -> Result<PathBuf> {
    let path = setup::expand(if host == "~" { "~/" } else { host });
    match fs::metadata(&path) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => bail!("{} is a file; vx mount shares folders", path.display()),
        Err(_) => return Err(hinted(format!("there's no folder at {}", path.display()), "create it first")),
    }
    fs::canonicalize(&path).with_context(|| format!("resolving {}", path.display()))
}

/// Where a folder goes in the VM when you don't say: the same place relative to your home,
/// or the same absolute path when it's outside your home.
fn default_guest(host: &Path, home: Option<&Path>) -> Result<String> {
    let home = home.and_then(|h| fs::canonicalize(h).ok().or(Some(h.to_path_buf())));
    match home.as_deref().map(|h| host.strip_prefix(h)) {
        Some(Ok(rel)) if rel.as_os_str().is_empty() => Err(hinted(
            "your whole home folder needs somewhere else to go in the VM",
            "say where after a colon, e.g. `vx mount <vm> ~:~/host`",
        )),
        Some(Ok(rel)) => Ok(format!("~/{}", rel.display())),
        _ => Ok(host.display().to_string()),
    }
}

/// Trailing slashes off, so `~/code/` and `~/code` are the same mount.
fn normalize(guest: &str) -> String {
    let trimmed = guest.trim_end_matches('/');
    if trimmed.is_empty() { guest.to_string() } else { trimmed.to_string() }
}

/// A place in the VM: absolute, or `~/…`, with nothing a shell or `..` could play tricks with.
pub fn check_guest(guest: &str) -> Result<()> {
    let ok = (guest.starts_with('/') || guest == "~" || guest.starts_with("~/"))
        && !guest.split('/').any(|part| part == "..")
        && !guest.chars().any(|c| c.is_control());
    if !ok {
        return Err(hinted(
            format!("`{guest}` isn't a place in the VM"),
            "use an absolute path like /srv/code, or one in your home there like ~/code",
        ));
    }
    if RESERVED.contains(&normalize(guest).as_str()) {
        return Err(hinted(
            format!("mounting over {guest} would break the VM"),
            "pick a folder of its own, e.g. ~/code or /srv/code",
        ));
    }
    Ok(())
}

/// How one mount is doing, as the agent last saw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "state", content = "why")]
pub enum Status {
    /// The VM is still booting, or it's being mounted.
    Waiting,
    Installing,
    Mounted,
    Failed(String),
}

impl Status {
    fn settled(&self) -> bool {
        matches!(self, Status::Mounted | Status::Failed(_))
    }
}

/// What the agent last wrote, by guest path; empty when no agent is running.
pub fn statuses(vm: &Vm) -> BTreeMap<String, Status> {
    if !agent_running(vm) {
        return BTreeMap::new();
    }
    fs::read_to_string(vm.path(STATUS)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn agent_running(vm: &Vm) -> bool {
    let Ok(file) = File::options().create(true).truncate(false).write(true).open(vm.path(AGENT_LOCK)) else {
        return false;
    };
    file.try_lock().is_err()
}

/// Start the agent for a running VM with mounts, unless one is already at work.
pub fn ensure_agent(vm: &Vm) -> Result<()> {
    if vm.spec.mounts.is_empty() || agent_running(vm) {
        return Ok(());
    }
    // Left by an agent that didn't get to clean up, and about to be wrong.
    let _ = fs::remove_file(vm.path(STATUS));
    let log = File::options().create(true).append(true).open(vm.path(LOG))?;
    // VX_HOME is the VM directory's grandparent: $VX_HOME/vms/<name>.
    let home = vm.dir.parent().and_then(Path::parent).context("VM directory has no VX_HOME")?;
    Command::new(std::env::current_exe()?)
        .args(["mount-agent", &vm.name])
        .env("VX_HOME", home)
        // Its own process group and no inherited pipes, so whoever started the VM (the
        // dashboard waits for `vx start`'s output to end) isn't kept waiting for it.
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .context("starting vx mount-agent")?;
    Ok(())
}

/// Stop the agent, and with it every mount's connection, and wait for it to be gone. For
/// `vx rm`, which can't delete the VM's directory while the agent is still writing to it.
pub fn stop_agent(vm: &Vm) {
    if !agent_running(vm) {
        return;
    }
    let pid = fs::read_to_string(vm.path(AGENT_LOCK)).ok().and_then(|p| p.trim().parse::<i32>().ok());
    if let Some(pid) = pid.filter(|&p| p > 0) {
        // SAFETY: kill(2) has no memory-safety preconditions. The agent leads its own
        // process group (`ensure_agent`), so this reaches its ssh and sftp-server too.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while agent_running(vm) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
}

/// Wait for the agent to settle the mounts `which` picks, showing what it's doing. Returns
/// the ones that failed, with why.
pub fn wait(vm: &Vm, which: impl Fn(&Mount) -> bool, timeout: Duration) -> Vec<(Mount, String)> {
    let wanted: Vec<&Mount> = vm.spec.mounts.iter().filter(|m| which(m)).collect();
    let mut spinner = Spinner::new();
    let started = Instant::now();
    let mut deadline = started + timeout;
    loop {
        let now = statuses(vm);
        let pending: Vec<&Mount> =
            wanted.iter().copied().filter(|m| !now.get(&m.guest).is_some_and(Status::settled)).collect();
        let installing = pending.iter().any(|m| now.get(&m.guest) == Some(&Status::Installing));
        if installing {
            deadline = deadline.max(Instant::now() + Duration::from_secs(60)); // a slow download
        }
        // A moment's grace for an agent that's only just been started.
        let gone = started.elapsed() > Duration::from_secs(2) && !agent_running(vm);
        if pending.is_empty() || gone || Instant::now() > deadline {
            spinner.clear();
            return wanted
                .into_iter()
                .filter_map(|m| match now.get(&m.guest) {
                    Some(Status::Mounted) => None,
                    Some(Status::Failed(why)) => Some((m.clone(), why.clone())),
                    _ if gone => Some((m.clone(), format!("see {}", vm.path(LOG).display()))),
                    _ => Some((m.clone(), format!("still working on it; `vx mount {}` shows when it's done", vm.name))),
                })
                .collect();
        }
        let label = if installing { "installing sshfs…" } else { "mounting…" };
        spinner.update(label, &pending.iter().map(|m| m.guest.as_str()).collect::<Vec<_>>().join(", "));
        thread::sleep(Duration::from_millis(100));
    }
}

/// Warn about mounts that didn't come up. Doesn't fail: the VM itself is fine.
pub fn report(vm: &Vm, failed: &[(Mount, String)]) {
    for (m, why) in failed {
        style::warn("", format!("{} isn't mounted in {} at {}: {why}", tilde(&m.host), vm.name, m.guest));
    }
}

/// `host → vm:guest`, with `~` for your home on this machine.
fn describe(vm: &Vm, m: &Mount) -> String {
    let ro = if m.read_only { " (read-only)" } else { "" };
    format!("{} → {}:{}{ro}", tilde(&m.host), vm.name, m.guest)
}

fn tilde(path: &Path) -> String {
    match std::env::home_dir().and_then(|h| path.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rel) if rel.as_os_str().is_empty() => "~".into(),
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

pub fn add(home: &Home, vm: &mut Vm, mounts: Vec<Mount>) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let mut added = Vec::new();
    {
        let _lock = vm.lock()?;
        for m in mounts {
            if vm.spec.mounts.contains(&m) {
                println!("{}", OUT.dim(format!("already mounted: {}", describe(vm, &m))));
                continue;
            }
            if let Some(other) = vm.spec.mounts.iter().find(|o| o.guest == m.guest) {
                return Err(hinted(
                    format!("{} already has {} mounted at {}", vm.name, tilde(&other.host), m.guest),
                    format!("remove it first with `vx mount rm {} {}`", vm.name, m.guest),
                ));
            }
            vm.spec.mounts.push(m.clone());
            added.push(m);
        }
        vm.save()?;
    }
    if added.is_empty() {
        return Ok(());
    }
    match backend.state(vm) {
        State::Running => {
            ssh::write_config(home, vm, backend.ssh_addr(vm))?;
            ensure_agent(vm)?;
            let failed = wait(vm, |m| added.contains(m), Duration::from_secs(60));
            for m in &added {
                if !failed.iter().any(|(f, _)| f == m) {
                    println!("{} {}", OUT.green('✓'), describe(vm, m));
                }
            }
            report(vm, &failed);
        }
        _ => {
            for m in &added {
                println!("{} {} {}", OUT.green('✓'), describe(vm, m), OUT.dim("(from when it starts)"));
            }
        }
    }
    Ok(())
}

/// Stop sharing the mounts `which` names, by their path here or in the VM.
pub fn remove(vm: &mut Vm, which: &[String]) -> Result<()> {
    let mut removed = Vec::new();
    {
        let _lock = vm.lock()?;
        for name in which {
            let host = resolve(name).ok();
            let guest = normalize(name);
            let Some(i) = vm.spec.mounts.iter().position(|m| m.guest == guest || Some(&m.host) == host.as_ref()) else {
                return Err(hinted(
                    format!("{} has nothing mounted at {name}", vm.name),
                    format!("vx mount {}", vm.name),
                ));
            };
            removed.push(vm.spec.mounts.remove(i));
        }
        vm.save()?;
    }
    // The agent unmounts them as soon as it sees vx.toml change.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && removed.iter().any(|m| statuses(vm).contains_key(&m.guest)) {
        thread::sleep(Duration::from_millis(100));
    }
    for m in &removed {
        println!("{} stopped sharing {}", OUT.green('✓'), describe(vm, m));
    }
    Ok(())
}

pub fn list(vm: &Vm) -> Result<()> {
    let state = backend::get(&vm.spec.backend)?.state(vm);
    if vm.spec.mounts.is_empty() {
        println!("{} has no folders mounted", vm.name);
        println!();
        println!("{}", OUT.dim(format!("share one with `vx mount {} ~/code`", vm.name)));
        return Ok(());
    }
    let now = statuses(vm);
    let rows: Vec<String> = vm.spec.mounts.iter().map(|m| describe(vm, m)).collect();
    let w = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0);
    for (m, row) in vm.spec.mounts.iter().zip(&rows) {
        let note = match (&state, now.get(&m.guest)) {
            (State::Stopped, _) => OUT.dim("mounts when it starts".to_string()).to_string(),
            (_, Some(Status::Mounted)) => OUT.green("mounted").to_string(),
            (_, Some(Status::Waiting)) => OUT.dim("mounting…".to_string()).to_string(),
            (_, Some(Status::Installing)) => OUT.dim("installing sshfs…".to_string()).to_string(),
            (_, Some(Status::Failed(why))) => OUT.red(format!("not mounted: {why}")).to_string(),
            (_, None) => OUT.yellow(format!("not mounted; restart {} or see {LOG}", vm.name)).to_string(),
        };
        println!("{row:w$}  {note}");
    }
    Ok(())
}

// The agent.

/// One mount's connection: sftp-server here, talking to sshfs in the VM through ssh.
struct Conn {
    mount: Mount,
    ssh: Child,
    sftp: Child,
    /// The last thing ssh or the VM side printed, which says why it ended.
    last_error: Arc<Mutex<String>>,
    mounted: bool,
    started: Instant,
}

impl Conn {
    fn stop(mut self) {
        let _ = self.ssh.kill();
        let _ = self.sftp.kill();
        let _ = self.ssh.wait();
        let _ = self.sftp.wait();
    }
}

/// `vx mount-agent <vm>`: keep the VM's mounts up until it stops. Started by `ensure_agent`.
pub fn agent(home: &Home, name: &str) -> Result<()> {
    let vm = home.load(name)?;
    let mut lock = File::options().create(true).truncate(false).write(true).open(vm.path(AGENT_LOCK))?;
    // An agent from before a restart may still be on its way out.
    lock.lock()?;
    // For `stop_agent`.
    lock.set_len(0)?;
    write!(lock, "{}", std::process::id())?;
    let backend = backend::get(&vm.spec.backend)?;
    let config = ssh::write_config(home, &vm, backend.ssh_addr(&vm))?;
    let sftp_server = sftp_server()?;
    let mut conns: HashMap<String, Conn> = HashMap::new();
    let mut statuses: BTreeMap<String, Status> = BTreeMap::new();
    let mut retry_at: HashMap<String, Instant> = HashMap::new();
    let mut booted = false;
    let mut checked = Instant::now() - STATE_EVERY;
    log(&format!("started for {name}"));
    loop {
        if checked.elapsed() >= STATE_EVERY {
            checked = Instant::now();
            if backend.state(&vm) == State::Stopped {
                break;
            }
        }
        let Ok(vm) = home.load(name) else { break }; // deleted, or vx.toml broken
        let wanted = vm.spec.mounts.clone();
        if wanted.is_empty() && conns.is_empty() {
            break;
        }

        // Unmount what's no longer wanted, or changed.
        let stale: Vec<String> =
            conns.iter().filter(|(_, c)| !wanted.contains(&c.mount)).map(|(g, _)| g.clone()).collect();
        for guest in stale {
            let conn = conns.remove(&guest).unwrap();
            let _ = guest_command(&config, &vm).arg(unmount_script(&guest)).status();
            conn.stop();
            statuses.remove(&guest);
            log(&format!("unmounted {guest}"));
        }
        statuses.retain(|g, _| wanted.iter().any(|m| &m.guest == g));

        if !booted {
            booted = ssh::reachable(&config, &vm);
            if !booted {
                for m in wanted.iter().filter(|m| !conns.contains_key(&m.guest)) {
                    statuses.insert(m.guest.clone(), Status::Waiting);
                }
                write_statuses(&vm, &statuses);
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        }

        // Notice connections that ended, and why.
        let mut install = false;
        for guest in conns.keys().cloned().collect::<Vec<_>>() {
            let conn = conns.get_mut(&guest).unwrap();
            let Ok(Some(exit)) = conn.ssh.try_wait() else { continue };
            let conn = conns.remove(&guest).unwrap();
            let why = conn.last_error.lock().unwrap().clone();
            let was_mounted = conn.mounted;
            conn.stop();
            if exit.code() == Some(NO_SSHFS) {
                install = true;
                continue; // tried again right after
            }
            if was_mounted {
                // It was up, so the VM rebooted or was restored: mount it again once it's back.
                log(&format!("{guest}: dropped ({why}); mounting it again"));
                statuses.insert(guest, Status::Waiting);
                booted = false;
                continue;
            }
            let why = if why.is_empty() { format!("sshfs exited ({exit})") } else { why };
            log(&format!("{guest}: {why}"));
            statuses.insert(guest.clone(), Status::Failed(why));
            retry_at.insert(guest, Instant::now() + RETRY_AFTER);
        }
        if !booted {
            write_statuses(&vm, &statuses);
            continue; // back to waiting for it to boot
        }
        if install {
            for m in &wanted {
                statuses.insert(m.guest.clone(), Status::Installing);
            }
            write_statuses(&vm, &statuses);
            if let Err(e) = install_sshfs(&config, &vm) {
                log(&format!("{e:#}"));
                let why = format!("couldn't install sshfs in {name}; try `vx install {name} sshfs`");
                for m in &wanted {
                    statuses.insert(m.guest.clone(), Status::Failed(why.clone()));
                    retry_at.insert(m.guest.clone(), Instant::now() + Duration::from_secs(120));
                }
            }
        }

        // Mount what isn't yet.
        for m in &wanted {
            if conns.contains_key(&m.guest) || retry_at.get(&m.guest).is_some_and(|t| Instant::now() < *t) {
                continue;
            }
            if !m.host.is_dir() {
                let why = format!("there's no folder at {} any more", m.host.display());
                statuses.insert(m.guest.clone(), Status::Failed(why));
                retry_at.insert(m.guest.clone(), Instant::now() + RETRY_AFTER);
                continue;
            }
            match connect(&config, &vm, &sftp_server, m) {
                Ok(conn) => {
                    if !matches!(statuses.get(&m.guest), Some(Status::Failed(_))) {
                        statuses.insert(m.guest.clone(), Status::Waiting);
                    }
                    conns.insert(m.guest.clone(), conn);
                }
                Err(e) => {
                    statuses.insert(m.guest.clone(), Status::Failed(format!("{e:#}")));
                    retry_at.insert(m.guest.clone(), Instant::now() + RETRY_AFTER);
                }
            }
        }

        // A connection that's still up after starting is mounted once the VM says so.
        let checking: Vec<String> = conns.iter().filter(|(_, c)| !c.mounted).map(|(g, _)| g.clone()).collect();
        if !checking.is_empty() {
            let mounted = mounted_in_guest(&config, &vm, &checking);
            for guest in checking {
                let conn = conns.get_mut(&guest).unwrap();
                if mounted.contains(&guest) {
                    conn.mounted = true;
                    retry_at.remove(&guest);
                    statuses.insert(guest.clone(), Status::Mounted);
                    log(&format!("mounted {guest}"));
                } else if conn.started.elapsed() > Duration::from_secs(30) {
                    let conn = conns.remove(&guest).unwrap();
                    conn.stop();
                    statuses.insert(guest.clone(), Status::Failed("sshfs didn't finish mounting".into()));
                    retry_at.insert(guest, Instant::now() + RETRY_AFTER);
                }
            }
        }
        write_statuses(&vm, &statuses);
        thread::sleep(Duration::from_millis(500));
    }
    for (_, conn) in conns.drain() {
        conn.stop();
    }
    let _ = fs::remove_file(vm.path(STATUS));
    log(&format!("{name} stopped; done"));
    Ok(())
}

/// The agent's stderr is mounts.log.
fn log(msg: &str) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ = writeln!(io::stderr(), "[{now}] {msg}");
}

fn write_statuses(vm: &Vm, statuses: &BTreeMap<String, Status>) {
    let tmp = vm.path(".mounts.json.tmp");
    if let Ok(text) = serde_json::to_string(statuses)
        && fs::write(&tmp, text).is_ok()
    {
        let _ = fs::rename(&tmp, vm.path(STATUS));
    }
}

/// Where this machine's OpenSSH keeps its SFTP server.
fn sftp_server() -> Result<PathBuf> {
    const PLACES: &[&str] = &[
        "/usr/libexec/sftp-server",         // macOS
        "/usr/lib/openssh/sftp-server",     // Debian, Ubuntu
        "/usr/libexec/openssh/sftp-server", // Fedora, RHEL
        "/usr/lib/ssh/sftp-server",         // Arch, openSUSE
        "/usr/lib/misc/sftp-server",        // Gentoo
    ];
    PLACES.iter().map(PathBuf::from).find(|p| p.is_file()).ok_or_else(|| {
        hinted(
            "OpenSSH's sftp-server isn't on this machine",
            "install openssh-server (Debian, Ubuntu) or openssh (Fedora)",
        )
    })
}

/// ssh into the VM to run one command, without asking anything. Keepalives notice a VM that
/// rebooted: QEMU's user network keeps the old connection open, so nothing else would.
fn guest_command(config: &Path, vm: &Vm) -> Command {
    let mut cmd = ssh::command(config, vm);
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "-o", "ServerAliveInterval=5", "--"]);
    cmd
}

/// Start sftp-server here and sshfs in the VM, each reading what the other writes.
fn connect(config: &Path, vm: &Vm, sftp_server: &Path, m: &Mount) -> Result<Conn> {
    let (from_sftp, to_ssh) = io::pipe()?;
    let (from_ssh, to_sftp) = io::pipe()?;
    let mut sftp = Command::new(sftp_server);
    if m.read_only {
        sftp.arg("-R");
    }
    let sftp = sftp.stdin(from_ssh).stdout(to_ssh).stderr(Stdio::null()).spawn().context("starting sftp-server")?;
    let mut ssh = match guest_command(config, vm)
        .arg(mount_script(m))
        .stdin(from_sftp)
        .stdout(to_sftp)
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh")
    {
        Ok(ssh) => ssh,
        Err(e) => {
            let mut sftp = sftp;
            let _ = sftp.kill();
            let _ = sftp.wait();
            return Err(e);
        }
    };
    let last_error = Arc::new(Mutex::new(String::new()));
    let stderr = ssh.stderr.take().unwrap();
    let (last, guest) = (last_error.clone(), m.guest.clone());
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let line = line.trim().to_string();
            if !line.is_empty() {
                log(&format!("{guest}: {line}"));
                *last.lock().unwrap() = line;
            }
        }
    });
    Ok(Conn { mount: m.clone(), ssh, sftp, last_error, mounted: false, started: Instant::now() })
}

/// `'…'` for sh.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A guest path for sh, with `~` as the user's home.
fn guest_path(guest: &str) -> String {
    match guest.strip_prefix('~') {
        Some("") => "\"$HOME\"".into(),
        Some(rest) => format!("\"$HOME\"{}", quote(rest)),
        None => quote(guest),
    }
}

/// Runs in the VM as the VM's user: mount `m` with sshfs, talking SFTP on stdin and stdout.
/// Runs until it's unmounted.
fn mount_script(m: &Mount) -> String {
    let ro = if m.read_only { ",ro" } else { "" };
    format!(
        r#"p={p}
command -v sshfs >/dev/null || exit {NO_SSHFS}
# So root, and containers, can use it too.
grep -qs '^user_allow_other' /etc/fuse.conf || echo user_allow_other | sudo -n tee -a /etc/fuse.conf >/dev/null
# Left over from a connection that dropped.
if mountpoint -q "$p"; then fusermount3 -uz "$p" 2>/dev/null || fusermount -uz "$p" 2>/dev/null || sudo -n umount -l "$p"; fi
if [ ! -d "$p" ] && ! mkdir -p "$p" 2>/dev/null; then sudo -n mkdir -p "$p" || exit 1; fi
[ -O "$p" ] || sudo -n chown "$(id -u):$(id -g)" "$p" || exit 1
# Your files here are yours there, group too. No directory cache: a file made on this
# machine shows up in the VM straight away.
exec sshfs -f -o passive,idmap=user,gid="$(id -g)",allow_other,dir_cache=no{ro} {host} "$p"
"#,
        p = guest_path(&m.guest),
        host = quote(&format!(":{}", m.host.display())),
    )
}

fn unmount_script(guest: &str) -> String {
    let p = guest_path(guest);
    format!("fusermount3 -uz {p} 2>/dev/null || fusermount -uz {p} 2>/dev/null || sudo -n umount -l {p}")
}

/// Which of `guests` the VM has a mount at.
fn mounted_in_guest(config: &Path, vm: &Vm, guests: &[String]) -> Vec<String> {
    let checks: Vec<String> =
        guests.iter().map(|g| format!("mountpoint -q {} && echo y || echo n", guest_path(g))).collect();
    let Ok(out) = guest_command(config, vm).arg(checks.join("; ")).stdin(Stdio::null()).output() else {
        return vec![];
    };
    let answers = String::from_utf8_lossy(&out.stdout);
    guests.iter().zip(answers.lines()).filter(|(_, a)| *a == "y").map(|(g, _)| g.clone()).collect()
}

fn install_sshfs(config: &Path, vm: &Vm) -> Result<()> {
    log("installing sshfs");
    let log = File::options().create(true).append(true).open(vm.path(LOG))?;
    let mut child = guest_command(config, vm)
        .arg("sudo -n sh -s")
        .stdin(Stdio::piped())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("running ssh")?;
    child.stdin.take().unwrap().write_all(setup::script(&["sshfs".into()], true).as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        bail!("installing sshfs failed ({status})");
    }
    Ok(())
}

/// Fails, saying what to install, if this machine can't share folders at all.
pub fn check_host() -> Result<()> {
    sftp_server().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_guest_paths() {
        let home = Path::new("/Users/me");
        let guest = |host: &str| default_guest(Path::new(host), Some(home));
        assert_eq!(guest("/Users/me/code").unwrap(), "~/code");
        assert_eq!(guest("/Users/me/src/vx").unwrap(), "~/src/vx");
        assert_eq!(guest("/Volumes/data").unwrap(), "/Volumes/data");
        assert!(guest("/Users/me").is_err(), "the whole home needs a place");
    }

    #[test]
    fn guest_paths() {
        for ok in ["~/code", "/srv/code", "/Users/me/x", "~/a b", "/mnt/it's"] {
            assert!(check_guest(ok).is_ok(), "{ok}");
        }
        for bad in ["code", "", "~code", "/srv/../etc", "~/..", "/", "/etc", "/etc/", "~", "~/", "~/.ssh", "/home"] {
            assert!(check_guest(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_mounts() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let m = parse(&format!("{}:/srv/x/", dir.display()), true).unwrap();
        assert_eq!(m, Mount { host: dir.clone(), guest: "/srv/x".into(), read_only: true });
        assert!(parse("/no/such/folder", false).is_err());
        assert!(parse(&format!("{}:etc", dir.display()), false).is_err());
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(guest_path("~"), "\"$HOME\"");
        assert_eq!(guest_path("~/my code"), "\"$HOME\"'/my code'");
        assert_eq!(guest_path("/srv/it's"), r"'/srv/it'\''s'");
    }

    #[test]
    fn mount_script_is_read_only_when_asked() {
        let m = Mount { host: "/Users/me/code".into(), guest: "~/code".into(), read_only: true };
        let script = mount_script(&m);
        assert!(
            script.contains(
                "-o passive,idmap=user,gid=\"$(id -g)\",allow_other,dir_cache=no,ro ':/Users/me/code' \"$p\""
            ),
            "{script}"
        );
        assert!(script.starts_with("p=\"$HOME\"'/code'\n"), "{script}");
    }
}
