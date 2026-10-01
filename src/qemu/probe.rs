//! What the QEMU command line needs to know about this machine.
//!
//! Probed fresh on every start and never stored: Homebrew paths contain the QEMU version,
//! and firmware files get renamed between releases.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use serde::Deserialize;

use crate::hinted;
use crate::host::{self, Arch, Os};

/// What QEMU finds in its own data directories when no descriptor names a firmware.
const DEFAULT_AARCH64_FIRMWARE: &str = "edk2-aarch64-code.fd";

#[derive(Debug)]
pub struct Host {
    pub os: Os,
    pub arch: Arch,
    /// `qemu-system-<guest arch>`.
    pub qemu: PathBuf,
    /// The hardware accelerator ("hvf" or "kvm"), or why there isn't one.
    pub accel: Result<&'static str, String>,
    /// UEFI firmware, for aarch64 guests only. x86_64 guests boot with QEMU's built-in SeaBIOS.
    pub firmware: Option<String>,
}

impl Host {
    pub fn probe(guest: Arch) -> Result<Host> {
        let os = Os::host();
        let arch = Arch::host()?;
        let qemu = find_qemu(os, arch, guest)?;
        let accel = native_accel(os, &qemu);
        let firmware = (guest == Arch::Aarch64).then(|| find_firmware(&qemu));
        Ok(Host { os, arch, qemu, accel, firmware })
    }
}

fn find_qemu(os: Os, arch: Arch, guest: Arch) -> Result<PathBuf> {
    let name = format!("qemu-system-{guest}");
    if let Some(path) = host::which(&name) {
        return Ok(path);
    }
    // The RHEL family ships only the native emulator, outside PATH.
    let rhel = Path::new("/usr/libexec/qemu-kvm");
    if os == Os::Linux && guest == arch && rhel.exists() {
        return Ok(rhel.into());
    }
    Err(hinted(format!("{name} not found"), install_hint(os)))
}

pub fn install_hint(os: Os) -> &'static str {
    match os {
        Os::Macos => "brew install qemu",
        Os::Linux => "install QEMU with your package manager, e.g. `sudo apt install qemu-system qemu-utils`",
    }
}

fn native_accel(os: Os, qemu: &Path) -> Result<&'static str, String> {
    match os {
        Os::Macos => {
            let hv_support = stdout("/usr/sbin/sysctl", ["-n", "kern.hv_support"]).is_some_and(|o| o.trim() == "1");
            let has_hvf = stdout(qemu, ["-accel", "help"]).is_some_and(|o| o.lines().any(|l| l.trim() == "hvf"));
            if !hv_support {
                Err("Hypervisor.framework is unavailable (are you inside a VM?)".into())
            } else if !has_hvf {
                Err("this QEMU was built without HVF; upgrade to macOS 15+, then `brew reinstall qemu`".into())
            } else {
                Ok("hvf")
            }
        }
        Os::Linux => match OpenOptions::new().read(true).write(true).open("/dev/kvm") {
            Ok(_) => Ok("kvm"),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                Err("no access to /dev/kvm; run `sudo usermod -aG kvm $USER`, then log out and back in".into())
            }
            Err(_) => Err("KVM is unavailable; enable VT-x/AMD-V in your firmware settings".into()),
        },
    }
}

/// Find aarch64 UEFI firmware through QEMU's firmware descriptors, the standard libvirt uses.
fn find_firmware(qemu: &Path) -> String {
    // Lowest priority first: a later directory overrides a file of the same name.
    let mut dirs: Vec<PathBuf> = stdout(qemu, ["-L", "help"])
        .unwrap_or_default()
        .lines()
        .map(|dir| Path::new(dir.trim()).join("firmware"))
        .collect();
    dirs.push("/usr/share/qemu/firmware".into());
    dirs.push("/etc/qemu/firmware".into());
    let config = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| env::home_dir().map(|h| h.join(".config")));
    dirs.extend(config.map(|c| c.join("qemu/firmware")));

    let mut files = BTreeMap::new();
    for dir in dirs {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension() == Some(OsStr::new("json")) {
                files.insert(entry.file_name(), path);
            }
        }
    }
    files
        .values()
        .find_map(|p| aarch64_uefi(&fs::read_to_string(p).ok()?))
        .unwrap_or_else(|| DEFAULT_AARCH64_FIRMWARE.into())
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Descriptor {
    interface_types: Vec<String>,
    mapping: Mapping,
    targets: Vec<Target>,
    #[serde(default)]
    features: Vec<String>,
}

#[derive(Deserialize)]
struct Mapping {
    device: String,
    /// For `"device": "flash"`.
    executable: Option<Image>,
    /// For `"device": "memory"`.
    filename: Option<String>,
}

#[derive(Deserialize)]
struct Image {
    filename: String,
    format: Option<String>,
}

#[derive(Deserialize)]
struct Target {
    architecture: String,
}

/// The firmware path, if this descriptor is a raw aarch64 UEFI build without secure boot.
fn aarch64_uefi(json: &str) -> Option<String> {
    let d: Descriptor = serde_json::from_str(json).ok()?;
    let usable = d.interface_types.iter().any(|t| t == "uefi")
        && d.targets.iter().any(|t| t.architecture == "aarch64")
        && !d.features.iter().any(|f| matches!(f.as_str(), "secure-boot" | "enrolled-keys" | "requires-smm"));
    if !usable {
        return None;
    }
    match d.mapping.device.as_str() {
        "flash" => {
            let exe = d.mapping.executable?;
            (exe.format.as_deref().unwrap_or("raw") == "raw").then_some(exe.filename)
        }
        "memory" => d.mapping.filename,
        _ => None,
    }
}

/// Run a command and return its stdout, or `None` if it couldn't run or failed.
fn stdout<const N: usize>(program: impl AsRef<OsStr>, args: [&str; N]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOMEBREW: &str = r#"{
        "description": "UEFI firmware for aarch64",
        "interface-types": ["uefi"],
        "mapping": {
            "device": "flash",
            "executable": { "filename": "/opt/homebrew/share/qemu/edk2-aarch64-code.fd", "format": "raw" },
            "nvram-template": { "filename": "/opt/homebrew/share/qemu/edk2-arm-vars.fd", "format": "raw" }
        },
        "targets": [{ "architecture": "aarch64", "machines": ["virt-*"] }],
        "features": ["verbose-static"],
        "tags": []
    }"#;

    #[test]
    fn reads_homebrew_descriptor() {
        assert_eq!(aarch64_uefi(HOMEBREW).as_deref(), Some("/opt/homebrew/share/qemu/edk2-aarch64-code.fd"));
    }

    #[test]
    fn skips_unusable_descriptors() {
        let bad = [
            HOMEBREW.replace(r#""verbose-static""#, r#""secure-boot""#),
            HOMEBREW.replace(r#""verbose-static""#, r#""enrolled-keys""#),
            HOMEBREW.replace(r#""format": "raw" },"#, r#""format": "qcow2" },"#),
            HOMEBREW.replace(r#""architecture": "aarch64""#, r#""architecture": "x86_64""#),
            HOMEBREW.replace(r#"["uefi"]"#, r#"["bios"]"#),
            "not json".into(),
        ];
        for json in bad {
            assert_eq!(aarch64_uefi(&json), None, "{json}");
        }
    }

    #[test]
    fn memory_mapping_and_default_format() {
        let json = r#"{
            "interface-types": ["uefi"],
            "mapping": { "device": "memory", "filename": "/usr/share/AAVMF/AAVMF_CODE.fd" },
            "targets": [{ "architecture": "aarch64" }]
        }"#;
        assert_eq!(aarch64_uefi(json).as_deref(), Some("/usr/share/AAVMF/AAVMF_CODE.fd"));
        let no_format = HOMEBREW.replace(r#", "format": "raw" },"#, " },");
        assert!(aarch64_uefi(&no_format).is_some());
    }
}
