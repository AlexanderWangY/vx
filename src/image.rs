//! Cloud images: the catalog, and downloads verified against the publisher's checksum file.
//!
//! The cache holds `images/<name>-<arch>-<first 12 hex of checksum>.qcow2`, so when a "latest"
//! URL starts serving a new build, it becomes a new file. VMs copy their disk out of the cache,
//! so deleting it is always safe.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File};
use std::os::unix::fs::DirBuilderExt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use sha2::Digest as _;
use sha2::{Sha256, Sha512};

use crate::hinted;
use crate::host::Arch;
use crate::progress::Bar;
use crate::style::{self, ERR};
use crate::vx::Home;

pub const DEFAULT: &str = "debian-13";

/// A cloud image published next to a checksum file.
pub struct Image {
    pub name: &'static str,
    pub title: &'static str,
    /// Approximate download size, for display.
    pub size_mb: u32,
    /// Where the image and its checksum file live.
    dir: &'static str,
    /// The image's file name. A `*` stands for a version, looked up in the checksum file.
    file: &'static str,
    /// The checksum file's name. A `*` stands for a version, looked up in the directory listing.
    sums: &'static str,
    /// How the publisher spells each architecture; `None` if it doesn't build for it.
    aarch64: Option<Spelling>,
    x86_64: Option<Spelling>,
}

/// Fills `{arch}` and `{sub}` in an image's URLs.
#[derive(Clone, Copy)]
struct Spelling {
    arch: &'static str,
    /// A path segment some publishers add per architecture.
    sub: &'static str,
}

const fn spell(arch: &'static str) -> Option<Spelling> {
    Some(Spelling { arch, sub: "" })
}

const fn spell_sub(arch: &'static str, sub: &'static str) -> Option<Spelling> {
    Some(Spelling { arch, sub })
}

/// Debian and Ubuntu spell architectures the dpkg way.
const ARM64: Option<Spelling> = spell("arm64");
const AMD64: Option<Spelling> = spell("amd64");
const AARCH64: Option<Spelling> = spell("aarch64");
const X86_64: Option<Spelling> = spell("x86_64");

pub const CATALOG: &[Image] = &[
    Image {
        name: "debian-13",
        title: "Debian 13 (trixie)",
        size_mb: 337,
        dir: "https://cloud.debian.org/images/cloud/trixie/latest/",
        file: "debian-13-genericcloud-{arch}.qcow2",
        sums: "SHA512SUMS",
        aarch64: ARM64,
        x86_64: AMD64,
    },
    Image {
        name: "debian-12",
        title: "Debian 12 (bookworm)",
        size_mb: 340,
        dir: "https://cloud.debian.org/images/cloud/bookworm/latest/",
        file: "debian-12-genericcloud-{arch}.qcow2",
        sums: "SHA512SUMS",
        aarch64: ARM64,
        x86_64: AMD64,
    },
    Image {
        name: "ubuntu-26.04",
        title: "Ubuntu 26.04 LTS",
        size_mb: 945,
        dir: "https://cloud-images.ubuntu.com/releases/resolute/release/",
        file: "ubuntu-26.04-server-cloudimg-{arch}.img",
        sums: "SHA256SUMS",
        aarch64: ARM64,
        x86_64: AMD64,
    },
    Image {
        name: "ubuntu-24.04",
        title: "Ubuntu 24.04 LTS",
        size_mb: 620,
        dir: "https://cloud-images.ubuntu.com/releases/noble/release/",
        file: "ubuntu-24.04-server-cloudimg-{arch}.img",
        sums: "SHA256SUMS",
        aarch64: ARM64,
        x86_64: AMD64,
    },
    Image {
        name: "ubuntu-22.04",
        title: "Ubuntu 22.04 LTS",
        size_mb: 705,
        dir: "https://cloud-images.ubuntu.com/releases/jammy/release/",
        file: "ubuntu-22.04-server-cloudimg-{arch}.img",
        sums: "SHA256SUMS",
        aarch64: ARM64,
        x86_64: AMD64,
    },
    Image {
        name: "fedora-44",
        title: "Fedora 44 Cloud",
        size_mb: 528,
        dir: "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/{arch}/images/",
        file: "Fedora-Cloud-Base-Generic-44-*.{arch}.qcow2",
        sums: "Fedora-Cloud-44-*-{arch}-CHECKSUM",
        aarch64: AARCH64,
        x86_64: X86_64,
    },
    Image {
        name: "centos-stream-10",
        title: "CentOS Stream 10",
        size_mb: 884,
        dir: "https://cloud.centos.org/centos/10-stream/{arch}/images/",
        file: "CentOS-Stream-GenericCloud-10-latest.{arch}.qcow2",
        sums: "CentOS-Stream-GenericCloud-10-latest.{arch}.qcow2.SHA256SUM",
        aarch64: AARCH64,
        x86_64: X86_64,
    },
    Image {
        name: "centos-stream-9",
        title: "CentOS Stream 9",
        size_mb: 1190,
        dir: "https://cloud.centos.org/centos/9-stream/{arch}/images/",
        file: "CentOS-Stream-GenericCloud-9-latest.{arch}.qcow2",
        sums: "CentOS-Stream-GenericCloud-9-latest.{arch}.qcow2.SHA256SUM",
        aarch64: AARCH64,
        x86_64: X86_64,
    },
    Image {
        name: "rocky-10",
        title: "Rocky Linux 10",
        size_mb: 469,
        dir: "https://dl.rockylinux.org/pub/rocky/10/images/{arch}/",
        file: "Rocky-10-GenericCloud-Base.latest.{arch}.qcow2",
        sums: "Rocky-10-GenericCloud-Base.latest.{arch}.qcow2.CHECKSUM",
        aarch64: AARCH64,
        x86_64: X86_64,
    },
    Image {
        name: "rocky-9",
        title: "Rocky Linux 9",
        size_mb: 519,
        dir: "https://dl.rockylinux.org/pub/rocky/9/images/{arch}/",
        file: "Rocky-9-GenericCloud-Base.latest.{arch}.qcow2",
        sums: "Rocky-9-GenericCloud-Base.latest.{arch}.qcow2.CHECKSUM",
        aarch64: AARCH64,
        x86_64: X86_64,
    },
    Image {
        name: "almalinux-10",
        title: "AlmaLinux 10",
        size_mb: 442,
        dir: "https://repo.almalinux.org/almalinux/10/cloud/{arch}/images/",
        file: "AlmaLinux-10-GenericCloud-latest.{arch}.qcow2",
        sums: "CHECKSUM",
        aarch64: AARCH64,
        x86_64: X86_64,
    },
    Image {
        name: "almalinux-9",
        title: "AlmaLinux 9",
        size_mb: 451,
        dir: "https://repo.almalinux.org/almalinux/9/cloud/{arch}/images/",
        file: "AlmaLinux-9-GenericCloud-latest.{arch}.qcow2",
        sums: "CHECKSUM",
        aarch64: AARCH64,
        x86_64: X86_64,
    },
    Image {
        name: "opensuse-leap-16.0",
        title: "openSUSE Leap 16.0",
        size_mb: 318,
        dir: "https://download.opensuse.org/distribution/leap/16.0/appliances/",
        file: "Leap-16.0-Minimal-VM.{arch}-Cloud.qcow2",
        sums: "Leap-16.0-Minimal-VM.{arch}-Cloud.qcow2.sha256",
        aarch64: AARCH64,
        x86_64: X86_64,
    },
    Image {
        name: "opensuse-tumbleweed",
        title: "openSUSE Tumbleweed",
        size_mb: 291,
        dir: "https://download.opensuse.org/{sub}tumbleweed/appliances/",
        file: "openSUSE-Tumbleweed-Minimal-VM.{arch}-Cloud.qcow2",
        sums: "openSUSE-Tumbleweed-Minimal-VM.{arch}-Cloud.qcow2.sha256",
        aarch64: spell_sub("aarch64", "ports/aarch64/"),
        x86_64: X86_64,
    },
    Image {
        name: "archlinux",
        title: "Arch Linux",
        size_mb: 578,
        dir: "https://geo.mirror.pkgbuild.com/images/latest/",
        file: "Arch-Linux-{arch}-cloudimg.qcow2",
        sums: "Arch-Linux-{arch}-cloudimg.qcow2.SHA256",
        aarch64: None,
        x86_64: X86_64,
    },
    Image {
        name: "amazonlinux-2023",
        title: "Amazon Linux 2023",
        size_mb: 2068,
        dir: "https://cdn.amazonlinux.com/al2023/os-images/latest/{sub}/",
        file: "al2023-kvm-*-{arch}.xfs.gpt.qcow2",
        sums: "SHA256SUMS",
        aarch64: spell_sub("arm64", "kvm-arm64"),
        x86_64: spell_sub("x86_64", "kvm"),
    },
];

pub fn find(name: &str) -> Option<&'static Image> {
    CATALOG.iter().find(|i| i.name == name)
}

/// What `vx new --image` refers to.
pub enum Source {
    Catalog(&'static Image),
    /// One you added with `vx images add`.
    Custom { name: String, path: PathBuf },
    File(PathBuf),
}

impl Source {
    /// A catalog name, the name of an image you added, or a path to a disk image.
    pub fn parse(home: &Home, arg: &str) -> Result<Source> {
        if let Some(image) = find(arg) {
            return Ok(Source::Catalog(image));
        }
        if let Some(path) = custom(home, arg) {
            return Ok(Source::Custom { name: arg.into(), path });
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
            Source::Custom { name, .. } => name.clone(),
            Source::File(path) => path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        }
    }

    pub fn title(&self) -> String {
        match self {
            Source::Catalog(image) => image.title.into(),
            Source::Custom { .. } | Source::File(_) => self.name(),
        }
    }

    /// Fail early if the image isn't built for `arch`.
    pub fn check_arch(&self, arch: Arch) -> Result<()> {
        match self {
            Source::Catalog(image) => image.spelling(arch).map(drop),
            Source::Custom { .. } | Source::File(_) => Ok(()),
        }
    }

    /// A local copy of the image: the file itself, or a verified download.
    pub fn fetch(&self, home: &Home, arch: Arch) -> Result<PathBuf> {
        match self {
            Source::Catalog(image) => image.fetch(home, arch),
            Source::Custom { path, .. } | Source::File(path) => Ok(path.clone()),
        }
    }
}

impl Image {
    pub fn supports(&self, arch: Arch) -> bool {
        self.spelling(arch).is_ok()
    }

    fn spelling(&self, arch: Arch) -> Result<Spelling> {
        let (spelling, other) = match arch {
            Arch::Aarch64 => (self.aarch64, Arch::X86_64),
            Arch::X86_64 => (self.x86_64, Arch::Aarch64),
        };
        spelling.ok_or_else(|| {
            hinted(
                format!("{} has no {arch} build", self.name),
                format!("add `--arch {other}` to run it emulated, which is much slower"),
            )
        })
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
        let spelling = self.spelling(arch)?;
        home.init()?;
        let (url, sum) = match self.resolve(spelling) {
            Ok(found) => found,
            Err(e) => {
                // Offline, or the mirror is down: the newest copy we have will do.
                if let Some(path) = self.cached(home, arch).into_iter().next() {
                    style::warn("  ", format!("couldn't check for a newer {} ({e:#}); using the cached copy", self.name));
                    return Ok(path);
                }
                return Err(e);
            }
        };

        let cache = |sum: &Checksum| home.images().join(format!("{}-{arch}-{}.qcow2", self.name, &sum.hex[..12]));
        let path = cache(&sum);
        if path.exists() {
            eprintln!("  {} {} {}", ERR.green('✓'), self.name, ERR.dim("(cached)"));
            return Ok(path);
        }
        match download(&url, &path, &sum, self.name) {
            // "latest" can change between fetching the checksum and the image; try once more.
            Err(e) if e.is::<Mismatch>() => {
                style::warn("  ", format!("{e}; retrying"));
                let (url, sum) = self.resolve(spelling)?;
                let path = cache(&sum);
                download(&url, &path, &sum, self.name)?;
                Ok(path)
            }
            Err(e) => Err(e),
            Ok(()) => Ok(path),
        }
    }

    /// The image's URL and its published checksum, looking up versions where the name has one.
    fn resolve(&self, spelling: Spelling) -> Result<(String, Checksum)> {
        let fill = |s: &str| s.replace("{arch}", spelling.arch).replace("{sub}", spelling.sub);
        let dir = fill(self.dir);

        let mut sums = fill(self.sums);
        if sums.contains('*') {
            let listing = get_text(&dir)?;
            sums = newest(hrefs(&listing), &sums).with_context(|| format!("{dir} has no {sums}"))?;
        }
        let sums_url = format!("{dir}{sums}");
        let file = fill(self.file);
        let (name, sum) = newest_sum(&get_text(&sums_url)?, &file)
            .with_context(|| format!("{sums_url} has no checksum for {file}"))?;
        Ok((format!("{dir}{name}"), sum))
    }
}

fn get_text(url: &str) -> Result<String> {
    agent().get(url).call().and_then(|mut r| r.body_mut().read_to_string()).map_err(|e| network_error(e, url))
}

/// Does `name` match `pattern`, where a single `*` matches anything?
fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len() && name.starts_with(prefix) && name.ends_with(suffix)
        }
    }
}

/// The last name matching `pattern`, in sort order.
fn newest(names: impl IntoIterator<Item = String>, pattern: &str) -> Option<String> {
    names.into_iter().filter(|n| glob(pattern, n)).max()
}

/// File names linked from an HTML directory listing.
fn hrefs(html: &str) -> Vec<String> {
    html.split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(|link| link.split(['?', '#']).next().unwrap_or_default())
        .filter(|link| !link.is_empty() && !link.ends_with('/'))
        .map(|link| link.rsplit('/').next().unwrap_or(link).to_string())
        .collect()
}

/// The downloads in the cache, including interrupted ones; images you added aren't included.
fn downloads(home: &Home) -> Vec<(PathBuf, fs::Metadata)> {
    fs::read_dir(home.images())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.path(), e.metadata().ok().filter(|m| m.is_file())?)))
        .collect()
}

/// Delete every cached download. Images you added stay. Returns the bytes freed.
pub fn prune(home: &Home) -> Result<u64> {
    let mut freed = 0;
    for (path, meta) in downloads(home) {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        freed += meta.len();
    }
    Ok(freed)
}

pub fn cache_size(home: &Home) -> u64 {
    downloads(home).iter().map(|(_, m)| m.len()).sum()
}

/// Images you added, kept apart from downloads so pruning the cache never touches them.
pub fn custom_dir(home: &Home) -> PathBuf {
    home.images().join("custom")
}

fn custom(home: &Home, name: &str) -> Option<PathBuf> {
    let path = custom_dir(home).join(format!("{name}.img"));
    path.is_file().then_some(path)
}

pub struct Custom {
    pub name: String,
    pub size: u64,
}

/// Images you added, sorted by name.
pub fn customs(home: &Home) -> Vec<Custom> {
    let mut found: Vec<Custom> = fs::read_dir(custom_dir(home))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.strip_suffix(".img")?.to_string();
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            Some(Custom { name, size: meta.len() })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Why `name` can't be used for a new custom image, if it can't.
pub fn check_custom_name(home: &Home, name: &str) -> Result<()> {
    let valid = name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'));
    if !valid {
        return Err(hinted(
            format!("`{name}` isn't a valid image name"),
            "use up to 32 lowercase letters, digits, `-` and `.`, starting with a letter",
        ));
    }
    if find(name).is_some() {
        return Err(hinted(format!("`{name}` is a built-in image"), "pick another name"));
    }
    if custom(home, name).is_some() {
        return Err(hinted(
            format!("there's already an image called `{name}`"),
            format!("pick another name, or delete it with `vx images rm {name}`"),
        ));
    }
    Ok(())
}

/// Add a disk image of your own (qcow2 or raw) under `name`. It's copied, which is instant
/// on APFS, btrfs and XFS, so the original can move or go away.
pub fn add_custom(home: &Home, name: &str, file: &Path) -> Result<PathBuf> {
    check_custom_name(home, name)?;
    if !file.is_file() {
        return Err(hinted(format!("{} isn't a file", file.display()), "pass the path to a qcow2 or raw disk image"));
    }
    let dir = custom_dir(home);
    DirBuilder::new().recursive(true).mode(0o700).create(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let part = dir.join(format!(".{name}.part"));
    let dest = dir.join(format!("{name}.img"));
    fs::copy(file, &part).with_context(|| format!("copying {}", file.display()))?;
    fs::rename(&part, &dest).with_context(|| format!("moving into {}", dest.display()))?;
    Ok(dest)
}

/// Delete an image's local copies: every cached download of a catalog image, or an image you
/// added. Returns the bytes freed.
pub fn remove(home: &Home, name: &str) -> Result<u64> {
    if let Some(path) = custom(home, name) {
        let size = path.metadata()?.len();
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        return Ok(size);
    }
    if find(name).is_none() {
        return Err(hinted(format!("no image called `{name}`"), "`vx images` lists them"));
    }
    let mut freed = 0;
    for (path, meta) in downloads(home) {
        if is_download_of(name, &path) {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            freed += meta.len();
        }
    }
    if freed == 0 {
        bail!("{name} isn't downloaded");
    }
    Ok(freed)
}

/// `<name>-<arch>-<hex>.qcow2`, or a `.part` of one.
fn is_download_of(name: &str, path: &Path) -> bool {
    let Some(file) = path.file_name().and_then(|f| f.to_str()) else { return false };
    file.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|rest| ["aarch64-", "x86_64-"].iter().any(|arch| rest.starts_with(arch)))
}

/// One image as `vx images` and the dashboard show it.
pub struct Info {
    pub name: String,
    pub title: String,
    /// Download size for catalog images, file size for ones you added.
    pub size: u64,
    /// Bytes of finished downloads for this arch, or the custom image's size.
    pub on_disk: u64,
    /// Bytes so far of a download in progress, from any `vx` process.
    pub downloading: Option<u64>,
    pub custom: bool,
    /// Built for the arch asked about.
    pub native: bool,
}

/// Every image, catalog first, for `arch`.
pub fn infos(home: &Home, arch: Arch) -> Vec<Info> {
    let files = downloads(home);
    let mut infos: Vec<Info> = CATALOG
        .iter()
        .map(|image| {
            let prefix = format!("{}-{arch}-", image.name);
            let mine = files.iter().filter(|(p, _)| p.file_name().and_then(|f| f.to_str()).is_some_and(|f| f.starts_with(&prefix)));
            let on_disk = mine.clone().filter(|(p, _)| p.extension() == Some(OsStr::new("qcow2"))).map(|(_, m)| m.len()).sum();
            // A .part that stopped growing is a download that was interrupted, not one in progress.
            let downloading = mine
                .filter(|(p, m)| {
                    p.extension() == Some(OsStr::new("part"))
                        && m.modified().is_ok_and(|t| t.elapsed().unwrap_or_default() < Duration::from_secs(5))
                })
                .map(|(_, m)| m.len())
                .max();
            Info {
                name: image.name.into(),
                title: image.title.into(),
                size: image.size_mb as u64 * 1_000_000,
                on_disk,
                downloading,
                custom: false,
                native: image.supports(arch),
            }
        })
        .collect();
    infos.extend(customs(home).into_iter().map(|c| Info {
        title: "added by you".into(),
        size: c.size,
        on_disk: c.size,
        downloading: None,
        custom: true,
        native: true,
        name: c.name,
    }));
    infos
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

/// Every (file, checksum) in a checksum file. Handles GNU (`<hex>  <file>`, `<hex> *<file>`)
/// and BSD (`SHA256 (<file>) = <hex>`) lines, and tells SHA-256 from SHA-512 by length.
fn parse_sums(text: &str) -> impl Iterator<Item = (String, Checksum)> {
    text.lines().filter_map(|line| {
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
        hex.bytes()
            .all(|b| b.is_ascii_hexdigit())
            .then(|| (name.to_string(), Checksum { alg, hex: hex.to_ascii_lowercase() }))
    })
}

/// The checksum of the last file matching `pattern`, in sort order.
fn newest_sum(text: &str, pattern: &str) -> Option<(String, Checksum)> {
    parse_sums(text).filter(|(name, _)| glob(pattern, name)).max_by(|a, b| a.0.cmp(&b.0))
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

    fn sum(text: &str, pattern: &str) -> Option<Checksum> {
        newest_sum(text, pattern).map(|(_, sum)| sum)
    }

    #[test]
    fn parses_gnu_sums() {
        let sha512 = "a".repeat(128);
        let text = format!(
            "{SHA256}  debian-13-genericcloud-amd64.json\n{sha512}  debian-13-genericcloud-arm64.qcow2\n"
        );
        let found = sum(&text, "debian-13-genericcloud-arm64.qcow2").unwrap();
        assert_eq!(found, Checksum { alg: Alg::Sha512, hex: sha512 });
    }

    #[test]
    fn parses_binary_marker_and_bsd_style() {
        let ubuntu = format!("{SHA256} *ubuntu-24.04-server-cloudimg-arm64.img\n");
        assert_eq!(sum(&ubuntu, "ubuntu-24.04-server-cloudimg-arm64.img").unwrap().alg, Alg::Sha256);
        let rocky = format!(
            "# Rocky-10-GenericCloud-Base.latest.aarch64.qcow2: 469368832 bytes\n\
             SHA256 (Rocky-10-GenericCloud-Base.latest.aarch64.qcow2) = {}\n",
            SHA256.to_uppercase()
        );
        assert_eq!(sum(&rocky, "Rocky-10-GenericCloud-Base.latest.aarch64.qcow2").unwrap().hex, SHA256);
    }

    #[test]
    fn ignores_other_files_and_bad_digests() {
        let text = format!("{SHA256}  other.qcow2\nnothex  disk.qcow2\n{}  disk.qcow2\n", "z".repeat(64));
        assert_eq!(sum(&text, "disk.qcow2"), None);
        // A file name that merely contains ours doesn't count.
        assert_eq!(sum(&format!("{SHA256}  disk.qcow2.sig\n"), "disk.qcow2"), None);
    }

    #[test]
    fn finds_versioned_names() {
        let fedora = format!(
            "SHA256 (Fedora-Cloud-Base-AmazonEC2-44-1.7.aarch64.raw.xz) = {SHA256}\n\
             SHA256 (Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2) = {SHA256}\n"
        );
        let (name, _) = newest_sum(&fedora, "Fedora-Cloud-Base-Generic-44-*.aarch64.qcow2").unwrap();
        assert_eq!(name, "Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2");

        let amazon = format!("{SHA256}  al2023-kvm-2023.12.20260930.0-kernel-6.1-arm64.xfs.gpt.qcow2\n");
        let (name, _) = newest_sum(&amazon, "al2023-kvm-*-arm64.xfs.gpt.qcow2").unwrap();
        assert_eq!(name, "al2023-kvm-2023.12.20260930.0-kernel-6.1-arm64.xfs.gpt.qcow2");
    }

    #[test]
    fn globs() {
        assert!(glob("a-*.qcow2", "a-1.7.qcow2"));
        assert!(glob("exact.qcow2", "exact.qcow2"));
        assert!(!glob("a-*.qcow2", "a-1.7.qcow2.sig"));
        assert!(!glob("ab*ba", "aba")); // prefix and suffix can't overlap
    }

    #[test]
    fn reads_directory_listings() {
        let apache = r#"<a href="?C=N;O=D">Name</a> <a href="../">Parent</a>
            <a href="Fedora-Cloud-44-1.7-aarch64-CHECKSUM">x</a> <a href="./Leap-16.0.qcow2">y</a>
            <a href="/pub/images/Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2">z</a>"#;
        assert_eq!(
            hrefs(apache),
            ["Fedora-Cloud-44-1.7-aarch64-CHECKSUM", "Leap-16.0.qcow2", "Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2"]
        );
        let found = newest(hrefs(apache), "Fedora-Cloud-44-*-aarch64-CHECKSUM");
        assert_eq!(found.as_deref(), Some("Fedora-Cloud-44-1.7-aarch64-CHECKSUM"));
    }

    #[test]
    fn catalog_is_consistent() {
        let mut names: Vec<_> = CATALOG.iter().map(|i| i.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), CATALOG.len(), "duplicate image names");
        assert!(CATALOG.iter().any(|i| i.name == DEFAULT));
        for image in CATALOG {
            assert!(image.aarch64.is_some() || image.x86_64.is_some(), "{}", image.name);
            assert!(image.dir.starts_with("https://") && image.dir.ends_with('/'), "{}", image.name);
            for template in [image.dir, image.file, image.sums] {
                assert!(template.matches('*').count() <= 1, "{}: {template}", image.name);
            }
        }
    }

    #[test]
    fn missing_arch_suggests_emulation() {
        let arch = CATALOG.iter().find(|i| i.name == "archlinux").unwrap();
        assert!(arch.supports(Arch::X86_64));
        let e = arch.spelling(Arch::Aarch64).err().unwrap();
        assert_eq!(e.to_string(), "archlinux has no aarch64 build");
    }

    #[test]
    fn hashes() {
        let mut h = Hasher::new(Alg::Sha256);
        h.update(b"abc");
        assert_eq!(h.hex(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    /// A throwaway $VX_HOME with some fake downloads in it.
    fn home_with_downloads() -> Home {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let home = Home::at(std::env::temp_dir().join(format!("vx-image-{}-{n}", std::process::id())));
        home.init().unwrap();
        for file in ["debian-13-aarch64-aaaaaaaaaaaa.qcow2", "debian-13-x86_64-bbbbbbbbbbbb.qcow2", "debian-12-aarch64-cccccccccccc.qcow2"] {
            fs::write(home.images().join(file), vec![0; 1000]).unwrap();
        }
        home
    }

    #[test]
    fn custom_images_are_added_used_and_removed() {
        let home = home_with_downloads();
        let file = home.root().join("disk.qcow2");
        fs::write(&file, vec![1; 500]).unwrap();

        add_custom(&home, "mine", &file).unwrap();
        assert!(matches!(Source::parse(&home, "mine").unwrap(), Source::Custom { ref name, .. } if name == "mine"));
        let infos = infos(&home, Arch::Aarch64);
        let mine = infos.iter().find(|i| i.name == "mine").unwrap();
        assert!(mine.custom && mine.on_disk == 500);
        assert_eq!(infos.iter().find(|i| i.name == "debian-13").unwrap().on_disk, 1000, "only this arch's copy");

        // Taken names, built-in names and bad names are refused, each with a hint.
        for bad in ["mine", "debian-13", "Mine", "1st"] {
            let e = add_custom(&home, bad, &file).unwrap_err();
            assert!(e.chain().any(|c| c.downcast_ref::<crate::Hinted>().is_some()), "{bad}: {e}");
        }

        // Pruning the cache leaves images you added alone.
        assert_eq!(prune(&home).unwrap(), 3000);
        assert!(custom(&home, "mine").is_some());
        assert_eq!(remove(&home, "mine").unwrap(), 500);
        assert!(custom(&home, "mine").is_none());
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn remove_deletes_only_that_images_downloads() {
        let home = home_with_downloads();
        fs::write(home.images().join("debian-13-aarch64-dddddddddddd.part"), vec![0; 10]).unwrap();
        assert_eq!(remove(&home, "debian-13").unwrap(), 2010);
        assert_eq!(cache_size(&home), 1000, "debian-12 is untouched");
        assert!(remove(&home, "debian-13").unwrap_err().to_string().contains("isn't downloaded"));
        assert!(remove(&home, "nope").is_err());
        fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn download_names() {
        let p = |f: &str| PathBuf::from(f);
        assert!(is_download_of("debian-13", &p("debian-13-aarch64-aaaaaaaaaaaa.qcow2")));
        assert!(is_download_of("debian-13", &p("debian-13-x86_64-aaaaaaaaaaaa.part")));
        assert!(!is_download_of("debian-1", &p("debian-13-aarch64-aaaaaaaaaaaa.qcow2")));
        assert!(!is_download_of("centos-stream-1", &p("centos-stream-10-aarch64-aaaaaaaaaaaa.qcow2")));
    }

    /// Hits the network: `cargo test -- --ignored catalog_resolves`. Reads only directory
    /// listings and checksum files, and sends a HEAD request for each image.
    #[test]
    #[ignore]
    fn catalog_resolves() {
        let mut failures = Vec::new();
        for image in CATALOG {
            for arch in [Arch::Aarch64, Arch::X86_64] {
                let Ok(spelling) = image.spelling(arch) else { continue };
                let result = image.resolve(spelling).and_then(|(url, sum)| {
                    let resp = agent().head(&url).call().map_err(|e| network_error(e, &url))?;
                    let mb = resp.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
                    Ok((url, sum, mb.unwrap_or(0) / 1_000_000))
                });
                match result {
                    Ok((url, sum, mb)) => println!("ok   {:20} {arch:8} {mb:>5} MB  {:?} {}…  {url}", image.name, sum.alg, &sum.hex[..12]),
                    Err(e) => failures.push(format!("{} {arch}: {e:#}", image.name)),
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }
}
