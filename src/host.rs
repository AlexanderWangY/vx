//! Facts about this machine that don't depend on any backend.

use std::env;
use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Macos,
    Linux,
}

impl Os {
    pub fn host() -> Os {
        if cfg!(target_os = "macos") { Os::Macos } else { Os::Linux }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    Aarch64,
    X86_64,
}

impl Arch {
    pub fn host() -> Result<Arch> {
        match env::consts::ARCH {
            "aarch64" => Ok(Arch::Aarch64),
            "x86_64" => Ok(Arch::X86_64),
            other => bail!("vx doesn't support {other} hosts"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Aarch64 => "aarch64",
            Arch::X86_64 => "x86_64",
        }
    }
}

impl FromStr for Arch {
    type Err = String;

    fn from_str(s: &str) -> Result<Arch, String> {
        match s {
            "aarch64" | "arm64" => Ok(Arch::Aarch64),
            "x86_64" | "amd64" => Ok(Arch::X86_64),
            _ => Err(format!("unknown architecture `{s}`; use aarch64 or x86_64")),
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The first executable called `name` on PATH.
/// This machine's memory, in bytes.
pub fn memory() -> Option<u64> {
    match Os::host() {
        Os::Macos => {
            let out =
                Command::new("/usr/sbin/sysctl").args(["-n", "hw.memsize"]).stderr(Stdio::null()).output().ok()?;
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()
        }
        Os::Linux => {
            let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
            let kb = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.trim().strip_suffix("kB")?;
            kb.trim().parse::<u64>().ok().map(|kb| kb * 1024)
        }
    }
}

pub fn which(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).map(|dir| dir.join(name)).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Run a command to completion and return its stdout. On failure, the error ends with
/// the last line the command printed to stderr.
pub fn run(cmd: &mut Command) -> Result<String> {
    let program = Path::new(cmd.get_program()).file_name().unwrap_or_default().to_string_lossy().into_owned();
    let out = cmd.stdin(Stdio::null()).output().with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let why = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("no error output");
        bail!("{program} failed: {why}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
