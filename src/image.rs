//! Cloud images: the catalog, and downloads verified against the publisher's checksum file.
//!
//! The cache holds `images/<name>-<arch>-<first 12 hex of checksum>.qcow2`, so when a "latest"
//! URL starts serving a new build, it becomes a new file. VMs copy their disk out of the cache,
//! so deleting it is always safe.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use sha2::Digest as _;
use sha2::{Sha256, Sha512};

use crate::hinted;
use crate::host::Arch;
use crate::progress::Bar;
use crate::vx::Home;

pub const DEFAULT: &str = "debian-13";

/// A cloud image published at a stable URL, next to a checksum file.
pub struct Image {
    pub name: &'static str,
    pub title: &'static str,
    /// Approximate download size, for display.
    pub size_mb: u32,
    /// The directory holding the image and its checksum file.
    dir: &'static str,
    /// The image's file name; `{arch}` becomes amd64 or arm64.
    file: &'static str,
    sums: &'static str,
}

pub const CATALOG: &[Image] = &[
    Image {
        name: "debian-13",
        title: "Debian 13 (trixie)",
        size_mb: 325,
        dir: "https://cloud.debian.org/images/cloud/trixie/latest/",
        file: "debian-13-genericcloud-{arch}.qcow2",
        sums: "SHA512SUMS",
    },
    Image {
        name: "ubuntu-24.04",
        title: "Ubuntu 24.04 LTS",
        size_mb: 595,
        dir: "https://cloud-images.ubuntu.com/releases/noble/release/",
        file: "ubuntu-24.04-server-cloudimg-{arch}.img",
        sums: "SHA256SUMS",
    },
    Image {
        name: "ubuntu-26.04",
        title: "Ubuntu 26.04 LTS",
        size_mb: 850,
        dir: "https://cloud-images.ubuntu.com/releases/resolute/release/",
        file: "ubuntu-26.04-server-cloudimg-{arch}.img",
        sums: "SHA256SUMS",
    },
];

/// What `vx new --image` refers to.
pub enum Source {
    Catalog(&'static Image),
    File(PathBuf),
}

impl Source {
    /// A catalog name, or a path to a disk image.
    pub fn parse(arg: &str) -> Result<Source> {
        if let Some(image) = CATALOG.iter().find(|i| i.name == arg) {
            return Ok(Source::Catalog(image));
        }
        let path = Path::new(arg);
        if path.is_file() {
            return Ok(Source::File(std::path::absolute(path)?));
        }
        Err(hinted(
            format!("no image called `{arg}`"),
            "`vx images` lists them; you can also pass a path to a qcow2 file",
        ))
    }

    pub fn name(&self) -> String {
        match self {
            Source::Catalog(image) => image.name.into(),
            Source::File(path) => path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        }
    }

    pub fn title(&self) -> String {
        match self {
            Source::Catalog(image) => image.title.into(),
            Source::File(_) => self.name(),
        }
    }

    /// A local copy of the image: the file itself, or a verified download.
    pub fn fetch(&self, home: &Home, arch: Arch) -> Result<PathBuf> {
        match self {
            Source::Catalog(image) => image.fetch(home, arch),
            Source::File(path) => Ok(path.clone()),
        }
    }
}

impl Image {
    fn file(&self, arch: Arch) -> String {
        let arch = match arch {
            Arch::Aarch64 => "arm64",
            Arch::X86_64 => "amd64",
        };
        self.file.replace("{arch}", arch)
    }

    /// Cached copies for `arch`, newest first.
    pub fn cached(&self, home: &Home, arch: Arch) -> Vec<PathBuf> {
        let prefix = format!("{}-{arch}-", self.name);
        let mut found: Vec<(SystemTime, PathBuf)> = fs::read_dir(home.images())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".qcow2")))
            .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), e.path()))
            .collect();
        found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        found.into_iter().map(|(_, path)| path).collect()
    }

    /// The current build of this image, from the cache or freshly downloaded and verified.
    pub fn fetch(&self, home: &Home, arch: Arch) -> Result<PathBuf> {
        home.init()?;
        let file = self.file(arch);
        let sum = match self.checksum(&file) {
            Ok(sum) => sum,
            Err(e) => {
                // Offline, or the mirror is down: the newest copy we have will do.
                if let Some(path) = self.cached(home, arch).into_iter().next() {
                    eprintln!("  warning: couldn't check for a newer {} ({e:#}); using the cached copy", self.name);
                    return Ok(path);
                }
                return Err(e);
            }
        };

        let path = home.images().join(format!("{}-{arch}-{}.qcow2", self.name, &sum.hex[..12]));
        if path.exists() {
            eprintln!("  ✓ {} (cached)", self.name);
            return Ok(path);
        }
        let url = format!("{}{file}", self.dir);
        match download(&url, &path, &sum, self.name) {
            // "latest" can change between fetching the checksum and the image; try once more.
            Err(e) if e.is::<Mismatch>() => {
                eprintln!("  {e}; retrying");
                let sum = self.checksum(&file)?;
                let path = home.images().join(format!("{}-{arch}-{}.qcow2", self.name, &sum.hex[..12]));
                download(&url, &path, &sum, self.name)?;
                Ok(path)
            }
            Err(e) => Err(e),
            Ok(()) => Ok(path),
        }
    }

    fn checksum(&self, file: &str) -> Result<Checksum> {
        let url = format!("{}{}", self.dir, self.sums);
        let text = agent()
            .get(&url)
            .call()
            .and_then(|mut r| r.body_mut().read_to_string())
            .map_err(|e| network_error(e, &url))?;
        parse_sums(&text, file).with_context(|| format!("{url} has no checksum for {file}"))
    }
}

/// Every file in the cache, including interrupted downloads. Returns the bytes freed.
pub fn prune(home: &Home) -> Result<u64> {
    let mut freed = 0;
    for entry in fs::read_dir(home.images()).into_iter().flatten() {
        let entry = entry?;
        freed += entry.metadata()?.len();
        fs::remove_file(entry.path()).with_context(|| format!("removing {}", entry.path().display()))?;
    }
    Ok(freed)
}

pub fn cache_size(home: &Home) -> u64 {
    fs::read_dir(home.images()).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Alg {
    Sha256,
    Sha512,
}

#[derive(Debug, PartialEq)]
struct Checksum {
    alg: Alg,
    hex: String,
}

/// Find `file` in a checksum file. Handles GNU (`<hex>  <file>`, `<hex> *<file>`) and
/// BSD (`SHA512 (<file>) = <hex>`) lines, and tells SHA-256 from SHA-512 by length.
fn parse_sums(text: &str, file: &str) -> Option<Checksum> {
    text.lines().find_map(|line| {
        let (name, hex) = match line.split_once(") = ") {
            Some((left, hex)) => (left.split_once(" (")?.1, hex.trim()),
            None => {
                let (hex, name) = line.split_once(char::is_whitespace)?;
                (name.trim().trim_start_matches('*'), hex)
            }
        };
        let alg = match hex.len() {
            64 => Alg::Sha256,
            128 => Alg::Sha512,
            _ => return None,
        };
        (name == file && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| Checksum { alg, hex: hex.to_ascii_lowercase() })
    })
}

#[derive(Debug)]
struct Mismatch(String);

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} didn't match its published checksum", self.0)
    }
}

impl std::error::Error for Mismatch {}

enum Hasher {
    Sha256(Sha256),
    Sha512(Sha512),
}

impl Hasher {
    fn new(alg: Alg) -> Hasher {
        match alg {
            Alg::Sha256 => Hasher::Sha256(Sha256::new()),
            Alg::Sha512 => Hasher::Sha512(Sha512::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(data),
            Hasher::Sha512(h) => h.update(data),
        }
    }

    fn hex(self) -> String {
        let digest = match self {
            Hasher::Sha256(h) => h.finalize().to_vec(),
            Hasher::Sha512(h) => h.finalize().to_vec(),
        };
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Stream `url` into `dest` while hashing it. The file only appears at `dest` once it matches.
fn download(url: &str, dest: &Path, want: &Checksum, label: &str) -> Result<()> {
    let part = dest.with_extension("part");
    let mut resp = agent().get(url).call().map_err(|e| network_error(e, url))?;
    let mut bar = Bar::new(label, resp.body().content_length());
    let mut body = resp.body_mut().as_reader();
    let mut file = File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let mut hasher = Hasher::new(want.alg);
    let mut buf = vec![0; 1 << 18];
    loop {
        let n = body.read(&mut buf).with_context(|| format!("downloading {url}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).with_context(|| format!("writing {}", part.display()))?;
        bar.add(n as u64);
    }
    file.sync_all()?;
    bar.finish();

    if hasher.hex() != want.hex {
        let _ = fs::remove_file(&part);
        return Err(Mismatch(label.into()).into());
    }
    fs::rename(&part, dest).with_context(|| format!("moving into {}", dest.display()))
}

fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig};
    ureq::Agent::config_builder()
        // The OS trust store, so a corporate TLS proxy's CA is trusted like in a browser.
        .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent(concat!("vx/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn network_error(e: ureq::Error, url: &str) -> anyhow::Error {
    let msg = format!("couldn't download {url}: {e}");
    let hint = match e {
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => {
            "if a corporate proxy inspects TLS, add its CA certificate to your system's trust store"
        }
        ureq::Error::StatusCode(_) => "the image may have moved; check the URL in a browser",
        _ => "check your internet connection; set HTTPS_PROXY if you need a proxy",
    };
    hinted(msg, hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parses_gnu_sums() {
        let sha512 = "a".repeat(128);
        let text = format!(
            "{SHA256}  debian-13-genericcloud-amd64.json\n{sha512}  debian-13-genericcloud-arm64.qcow2\n"
        );
        let sum = parse_sums(&text, "debian-13-genericcloud-arm64.qcow2").unwrap();
        assert_eq!(sum, Checksum { alg: Alg::Sha512, hex: sha512 });
    }

    #[test]
    fn parses_binary_marker_and_bsd_style() {
        let ubuntu = format!("{SHA256} *ubuntu-24.04-server-cloudimg-arm64.img\n");
        assert_eq!(parse_sums(&ubuntu, "ubuntu-24.04-server-cloudimg-arm64.img").unwrap().alg, Alg::Sha256);
        let bsd = format!("SHA256 (disk.qcow2) = {}\n", SHA256.to_uppercase());
        assert_eq!(parse_sums(&bsd, "disk.qcow2").unwrap().hex, SHA256);
    }

    #[test]
    fn ignores_other_files_and_bad_digests() {
        let text = format!("{SHA256}  other.qcow2\nnothex  disk.qcow2\n{}  disk.qcow2\n", "z".repeat(64));
        assert_eq!(parse_sums(&text, "disk.qcow2"), None);
        // A file name that merely contains ours doesn't count.
        assert_eq!(parse_sums(&format!("{SHA256}  disk.qcow2.sig\n"), "disk.qcow2"), None);
    }

    #[test]
    fn hashes() {
        let mut h = Hasher::new(Alg::Sha256);
        h.update(b"abc");
        assert_eq!(h.hex(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn file_names_per_arch() {
        assert_eq!(CATALOG[0].file(Arch::Aarch64), "debian-13-genericcloud-arm64.qcow2");
        assert_eq!(CATALOG[1].file(Arch::X86_64), "ubuntu-24.04-server-cloudimg-amd64.img");
    }
}
