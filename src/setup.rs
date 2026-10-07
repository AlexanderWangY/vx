//! Setting a new VM up the way you like it: packages, then your own script.
//!
//! What to install comes from `vx new --install`, plus the defaults in $VX_HOME/config.toml:
//!
//! ```toml
//! [new]
//! install = ["git", "build-tools"]
//! setup = "~/.vx/setup.sh"
//! ```
//!
//! Package names are the distro's own, except for a few that differ between distros
//! (`build-tools`, `python`, …), which are translated for whichever package manager the VM has.
//! The translation runs inside the VM, so it works for images vx doesn't know too.
//!
//! Both run over SSH once the VM is up, so their progress shows as it happens and what they
//! print is kept in the VM's setup.log.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::hinted;
use crate::progress::Spinner;
use crate::ssh;
use crate::style::ERR;
use crate::vx::{Home, Vm};

const CONFIG: &str = "config.toml";
/// Present while a VM is being set up, so the dashboard can say so.
pub const MARKER: &str = ".setting-up";

/// $VX_HOME/config.toml.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub new: Defaults,
}

/// What every new VM gets, unless `vx new --bare`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Packages, by the names `vx install` takes.
    #[serde(default)]
    pub install: Vec<String>,
    /// A script on this machine, run in the VM as you once the packages are in.
    #[serde(default)]
    pub setup: Option<String>,
}

impl Config {
    pub fn path(home: &Home) -> PathBuf {
        home.root().join(CONFIG)
    }

    pub fn load(home: &Home) -> Result<Config> {
        let path = Config::path(home);
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

/// What to do to a new VM once it's up.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Plan {
    pub install: Vec<String>,
    /// As written, e.g. `~/.vx/setup.sh`.
    pub setup: Option<String>,
}

impl Plan {
    /// The defaults (unless `bare`), plus what was asked for on top of them.
    pub fn new(defaults: &Defaults, bare: bool, install: &[String], setup: Option<&str>) -> Plan {
        let mut plan = if bare {
            Plan::default()
        } else {
            Plan { install: defaults.install.clone(), setup: defaults.setup.clone() }
        };
        for package in install {
            if !plan.install.contains(package) {
                plan.install.push(package.clone());
            }
        }
        if let Some(setup) = setup {
            plan.setup = Some(setup.into());
        }
        plan
    }

    /// Check what can be checked before creating the VM: the names, and that the script exists.
    pub fn check(&self) -> Result<()> {
        for package in &self.install {
            check_package(package)?;
        }
        if let Some(setup) = &self.setup {
            let path = expand(setup);
            if !path.is_file() {
                return Err(hinted(
                    format!("setup script {} doesn't exist", path.display()),
                    format!("create it, or change `setup` in {CONFIG}"),
                ));
            }
        }
        Ok(())
    }
}

/// `~/x` → $HOME/x.
pub fn expand(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// Package names reach a shell command line, so they're held to what package managers use.
pub fn check_package(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '@' | ':'));
    if !ok {
        return Err(hinted(format!("`{name}` isn't a package name"), "e.g. git, build-tools, python3"));
    }
    Ok(())
}

/// The package managers vx can drive, in the order the script looks for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Apt,
    Dnf,
    Zypper,
    Pacman,
}

const FAMILIES: [Family; 4] = [Family::Apt, Family::Dnf, Family::Zypper, Family::Pacman];

/// Names that differ between distros: any of `names` installs these packages, by family
/// (apt, dnf, zypper, pacman). Everything else is passed on as it is.
const ALIASES: &[(&[&str], [&[&str]; 4])] = &[
    (
        &["build-tools", "build-essential", "base-devel"],
        [&["build-essential"], &["gcc", "gcc-c++", "make"], &["gcc", "gcc-c++", "make"], &["base-devel"]],
    ),
    (
        &["python", "python3"],
        [
            &["python3", "python3-pip", "python3-venv"],
            &["python3", "python3-pip"],
            &["python3", "python3-pip"],
            &["python", "python-pip"],
        ],
    ),
    (
        &["node", "nodejs"],
        [&["nodejs", "npm"], &["nodejs", "npm"], &["nodejs-default", "npm-default"], &["nodejs", "npm"]],
    ),
    (&["go", "golang"], [&["golang"], &["golang"], &["go"], &["go"]]),
    (&["rust"], [&["rustc", "cargo"], &["rust", "cargo"], &["rust", "cargo"], &["rust"]]),
    (&["fd", "fd-find"], [&["fd-find"], &["fd-find"], &["fd"], &["fd"]]),
    (&["sshfs", "fuse-sshfs"], [&["sshfs"], &["fuse-sshfs"], &["sshfs"], &["sshfs"]]),
];

/// The packages to ask `family` for.
fn translate(packages: &[String], family: Family) -> Vec<String> {
    let i = FAMILIES.iter().position(|f| *f == family).unwrap();
    let mut out: Vec<String> = Vec::new();
    for package in packages {
        let names = ALIASES.iter().find(|(names, _)| names.contains(&package.as_str()));
        let wanted: Vec<String> = match names {
            Some((_, by_family)) => by_family[i].iter().map(|s| s.to_string()).collect(),
            None => vec![package.clone()],
        };
        for name in wanted {
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// A POSIX shell script, run as root, that installs `packages` with whichever package manager
/// the VM has. Lines starting `vx:` are progress for the spinner. `lean` leaves out what they
/// only recommend: sshfs on Fedora would otherwise bring a desktop's worth of GTK with it.
pub fn script(packages: &[String], lean: bool) -> String {
    let list = |family| translate(packages, family).join(" ");
    let (apt_lean, dnf_lean, zypper_lean) = if lean {
        (" --no-install-recommends", " --setopt=install_weak_deps=False", " --no-recommends")
    } else {
        ("", "", "")
    };
    format!(
        r#"set -e
has() {{ command -v "$1" >/dev/null 2>&1; }}
if has apt-get; then
  export DEBIAN_FRONTEND=noninteractive
  # A fresh Ubuntu may still be running its own first-boot updates; wait for them.
  apt="apt-get -o DPkg::Lock::Timeout=600 -q"
  echo "vx: updating package lists"
  $apt update
  echo "vx: installing {apt}"
  $apt install -y{apt_lean} {apt}
elif has dnf; then
  echo "vx: installing {dnf}"
  if ! dnf install -y{dnf_lean} {dnf}; then
    # Rocky, Alma and CentOS keep many everyday tools in EPEL.
    . /etc/os-release
    case " $ID $ID_LIKE " in
      *" rhel "*) echo "vx: trying again with EPEL"; dnf install -y epel-release; dnf install -y{dnf_lean} {dnf} ;;
      *) exit 1 ;;
    esac
  fi
elif has zypper; then
  echo "vx: installing {zypper}"
  zypper --non-interactive install{zypper_lean} {zypper}
elif has pacman; then
  echo "vx: installing {pacman}"
  pacman -Syu --noconfirm --needed {pacman}
else
  echo "vx: this VM has none of apt-get, dnf, zypper or pacman" >&2
  exit 1
fi
"#,
        apt = list(Family::Apt),
        dnf = list(Family::Dnf),
        zypper = list(Family::Zypper),
        pacman = list(Family::Pacman),
    )
}

/// Install `packages` in `vm`, which must accept SSH logins, showing progress as it goes.
pub fn install(config: &Path, vm: &Vm, packages: &[String]) -> Result<()> {
    for package in packages {
        check_package(package)?;
    }
    let what = packages.join(", ");
    let started = Instant::now();
    run(config, vm, "sudo -n sh -s", script(packages, false).as_bytes(), &format!("installing {what}…"))
        .map_err(|why| failed(vm, &format!("couldn't install {what} in {}", vm.name), why))?;
    done(&format!("installed {what}"), started);
    Ok(())
}

/// Run the setup script at `path` (on this machine) in `vm` as the VM's user, from their home.
pub fn run_script(config: &Path, vm: &Vm, path: &str) -> Result<()> {
    let local = expand(path);
    let body = fs::read(&local).with_context(|| format!("reading {}", local.display()))?;
    let name = local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.into());
    let started = Instant::now();
    // Copied over and run as a file, so its #! line decides what runs it.
    let remote = format!(
        "f=$(mktemp) && cat > \"$f\" && chmod +x \"$f\" && VX_VM={} \"$f\"; s=$?; rm -f \"$f\"; exit $s",
        vm.name
    );
    run(config, vm, &remote, &body, &format!("running {name}…"))
        .map_err(|why| failed(vm, &format!("{name} failed in {}", vm.name), why))?;
    done(&format!("ran {name}"), started);
    Ok(())
}

fn done(what: &str, started: Instant) {
    eprintln!("  {} {what} {}", ERR.green('✓'), ERR.dim(format!("({} s)", started.elapsed().as_secs())));
}

/// An error saying the VM itself is fine, and where the full output is.
fn failed(vm: &Vm, msg: &str, why: String) -> anyhow::Error {
    hinted(
        format!("{msg}: {why}"),
        format!("{} itself is fine; the full output is in {}", vm.name, vm.path("setup.log").display()),
    )
}

/// Run `remote` in the VM with `input` on its stdin, showing its latest output next to a
/// spinner and keeping all of it in setup.log. On failure, returns its last line.
fn run(config: &Path, vm: &Vm, remote: &str, input: &[u8], msg: &str) -> Result<(), String> {
    let log_path = vm.path("setup.log");
    let mut log = File::options().create(true).append(true).open(&log_path).map_err(|e| e.to_string())?;
    let _ = writeln!(log, "\n== {msg}");
    let _ = File::create(vm.path(MARKER));
    let result = stream(config, vm, remote, input, msg, &mut log);
    let _ = fs::remove_file(vm.path(MARKER));
    result
}

fn stream(config: &Path, vm: &Vm, remote: &str, input: &[u8], msg: &str, log: &mut File) -> Result<(), String> {
    let mut child = Command::new("ssh")
        .arg("-F")
        .arg(config)
        .args(["-o", "BatchMode=yes", "-o", "ServerAliveInterval=15"])
        .arg(ssh::alias(&vm.name))
        .arg(format!("exec 2>&1; {remote}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running ssh: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    // Both streams, line by line, onto one channel.
    let (tx, rx) = mpsc::channel();
    for out in [
        Box::new(child.stdout.take().expect("piped")) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().expect("piped")),
    ] {
        let tx = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let mut spinner = Spinner::new();
    let mut last = String::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let _ = writeln!(log, "{line}");
                let line = crate::progress::plain(&line).trim().to_string();
                if !line.is_empty() {
                    last = line;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        spinner.update(msg, &last);
    }
    spinner.clear();
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else if last.is_empty() {
        Err(format!("exit status {}", status.code().unwrap_or(-1)))
    } else {
        Err(last.trim_start_matches("vx: ").to_string())
    }
}

/// After `vx new --install`, when there are no defaults yet: how to make them the default.
pub fn tip(install: &[String]) -> String {
    let list: Vec<String> = install.iter().map(|p| format!("\"{p}\"")).collect();
    format!(
        "to install these in every new VM, put this in ~/.vx/{CONFIG}:\n    [new]\n    install = [{}]",
        list.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn names_are_translated_per_family() {
        let wanted = strings(&["git", "build-tools", "python", "build-essential", "htop"]);
        assert_eq!(
            translate(&wanted, Family::Apt),
            strings(&["git", "build-essential", "python3", "python3-pip", "python3-venv", "htop"])
        );
        assert_eq!(
            translate(&wanted, Family::Dnf),
            strings(&["git", "gcc", "gcc-c++", "make", "python3", "python3-pip", "htop"])
        );
        assert_eq!(translate(&wanted, Family::Pacman), strings(&["git", "base-devel", "python", "python-pip", "htop"]));
    }

    #[test]
    fn the_script_has_a_list_per_package_manager() {
        let script = script(&strings(&["git", "build-tools"]), false);
        assert!(script.contains("$apt install -y git build-essential"), "{script}");
        assert!(script.contains("dnf install -y git gcc gcc-c++ make"), "{script}");
        assert!(script.contains("zypper --non-interactive install git gcc gcc-c++ make"), "{script}");
        assert!(script.contains("pacman -Syu --noconfirm --needed git base-devel"), "{script}");
    }

    #[test]
    fn a_lean_script_skips_recommends() {
        let script = script(&strings(&["sshfs"]), true);
        assert!(script.contains("$apt install -y --no-install-recommends sshfs"), "{script}");
        assert!(script.contains("dnf install -y --setopt=install_weak_deps=False fuse-sshfs"), "{script}");
        assert!(script.contains("zypper --non-interactive install --no-recommends sshfs"), "{script}");
    }

    #[test]
    fn plans_combine_defaults_and_flags() {
        let defaults = Defaults { install: strings(&["git", "tmux"]), setup: Some("~/.vx/setup.sh".into()) };
        let plan = Plan::new(&defaults, false, &strings(&["tmux", "htop"]), None);
        assert_eq!(plan.install, strings(&["git", "tmux", "htop"]));
        assert_eq!(plan.setup.as_deref(), Some("~/.vx/setup.sh"));
        let bare = Plan::new(&defaults, true, &strings(&["htop"]), Some("./other.sh"));
        assert_eq!(bare, Plan { install: strings(&["htop"]), setup: Some("./other.sh".into()) });
        assert_eq!(Plan::new(&defaults, true, &[], None), Plan::default());
    }

    #[test]
    fn package_names_are_checked() {
        for good in ["git", "gcc-c++", "python3.12", "libfoo1:amd64", "@development-tools"] {
            assert!(check_package(good).is_ok(), "{good}");
        }
        for bad in ["", "-y", "a b", "x;rm", "$(id)", "a'b"] {
            assert!(check_package(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn config_reads_defaults() {
        let config: Config = toml::from_str("[new]\ninstall = [\"git\"]\nsetup = \"~/s.sh\"\n").unwrap();
        assert_eq!(config.new.install, ["git"]);
        assert!(toml::from_str::<Config>("[new]\ninstal = []\n").is_err(), "typos are caught");
        assert_eq!(toml::from_str::<Config>("").unwrap(), Config::default());
    }
}
