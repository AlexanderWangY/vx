// Parts of these aren't used by any command yet.
#[allow(dead_code)]
mod backend;
mod console;
#[allow(dead_code)]
mod host;
mod image;
mod progress;
#[allow(dead_code)]
mod qemu;
mod qmp;
mod seed;
mod ssh;
#[allow(dead_code)]
mod vx;

use std::fmt;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use backend::State;
use host::Arch;
use image::Source;
use vx::{Home, Spec, Vm};

/// Zero-config Linux VMs from the terminal.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create and boot a new VM
    New(NewArgs),
    /// List VMs
    Ls,
    /// Start a VM
    Start { name: String },
    /// Stop a VM: power button first, then force it off
    Stop {
        name: String,
        /// Skip the power button and force it off right away
        #[arg(long)]
        force: bool,
    },
    /// SSH into a VM, starting it first if needed
    Ssh {
        name: String,
        /// A command to run instead of a shell
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Attach to a VM's serial console (Ctrl-] to detach)
    Console { name: String },
    /// Print a VM's boot log
    Logs {
        name: String,
        /// Keep printing new output
        #[arg(short, long)]
        follow: bool,
    },
    /// Stop and delete a VM
    Rm { name: String },
    /// List the images `vx new` can use
    Images {
        /// Delete every cached download (VMs keep their own disks)
        #[arg(long)]
        prune: bool,
    },
}

#[derive(clap::Args)]
struct NewArgs {
    name: String,
    /// An image from `vx images`, or a path to a qcow2 file
    #[arg(long, default_value = image::DEFAULT)]
    image: String,
    /// Virtual CPUs [default: 4, or fewer on smaller hosts]
    #[arg(long)]
    cpus: Option<u32>,
    /// Memory, e.g. 4G or 512M [default: 4G]
    #[arg(long, value_parser = size)]
    mem: Option<String>,
    /// Disk size, e.g. 20G
    #[arg(long, value_parser = size, default_value = "20G")]
    disk: String,
    /// Guest architecture: aarch64 or x86_64. Another arch than the host's is emulated (slow).
    #[arg(long)]
    arch: Option<Arch>,
    /// Create the VM without starting it
    #[arg(long)]
    no_start: bool,
}

fn size(s: &str) -> Result<String, String> {
    if vx::is_size(s) { Ok(s.into()) } else { Err("use a size like 4G, 512M or 4096".into()) }
}

/// An error that ends with a `hint:` line telling the user how to fix it.
#[derive(Debug)]
pub struct Hinted {
    msg: String,
    hint: String,
}

impl fmt::Display for Hinted {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Hinted {}

pub fn hinted(msg: impl Into<String>, hint: impl Into<String>) -> anyhow::Error {
    Hinted { msg: msg.into(), hint: hint.into() }.into()
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        if let Some(h) = e.chain().find_map(|c| c.downcast_ref::<Hinted>()) {
            eprintln!("hint: {}", h.hint);
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let Some(command) = cli.command else {
        println!("dashboard");
        return Ok(());
    };
    let home = Home::from_env()?;
    match command {
        Command::New(args) => new(&home, args)?,
        Command::Ls => ls(&home)?,
        Command::Start { name } => start(&home.load(&name)?)?,
        Command::Stop { name, force } => stop(&home.load(&name)?, force)?,
        Command::Ssh { name, command } => ssh(&home, &home.load(&name)?, &command)?,
        Command::Console { name } => attach(&home.load(&name)?)?,
        Command::Logs { name, follow } => console::logs(&home.load(&name)?, follow)?,
        Command::Rm { name } => println!("rm {name}"),
        Command::Images { prune } => images(&home, prune)?,
    }
    Ok(())
}

fn ls(home: &Home) -> Result<()> {
    let names = home.names()?;
    if names.is_empty() {
        println!("no VMs yet; create one with `vx new <name>`");
        return Ok(());
    }
    let w = names.iter().map(String::len).max().unwrap_or(0).max("NAME".len());
    println!("{:w$}  {:8}  {:7}  {:>4}  {:>6}  SSH", "NAME", "STATE", "ARCH", "CPUS", "MEMORY");
    for name in names {
        match home.load(&name) {
            Ok(vm) => {
                let s = &vm.spec;
                let state = match backend::get(&s.backend) {
                    Ok(b) => b.state(&vm).to_string(),
                    Err(_) => format!("unknown backend `{}`", s.backend),
                };
                println!("{name:w$}  {state:8}  {:7}  {:>4}  {:>6}  127.0.0.1:{}", s.arch, s.cpus, s.memory, s.ssh.port);
            }
            Err(e) => println!("{name:w$}  error: {e:#}"),
        }
    }
    Ok(())
}

fn start(vm: &Vm) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let _lock = vm.lock()?;
    match backend.state(vm) {
        State::Stopped => {}
        state => bail!("{} is already {state}", vm.name),
    }
    backend.start(vm)?;
    println!("{} started; watch it boot with `vx console {0}` or `vx logs -f {0}`", vm.name);
    Ok(())
}

fn stop(vm: &Vm, force: bool) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let _lock = vm.lock()?;
    if backend.state(vm) == State::Stopped {
        println!("{} is already stopped", vm.name);
        return Ok(());
    }
    backend.stop(vm, force)?;
    println!("{} stopped", vm.name);
    Ok(())
}

fn attach(vm: &Vm) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let Some(c) = backend.console() else {
        bail!("the {} backend has no serial console", vm.spec.backend);
    };
    if backend.state(vm) == State::Stopped {
        return Err(hinted(format!("{} isn't running", vm.name), format!("vx start {}", vm.name)));
    }
    console::attach(&vm.name, c.attach(vm)?)
}

fn new(home: &Home, args: NewArgs) -> Result<()> {
    let started = Instant::now();
    home.check_new_name(&args.name)?;
    let source = Source::parse(&args.image)?;
    let mut spec = Spec::defaults()?;
    spec.image = source.name();
    spec.arch = args.arch.unwrap_or(spec.arch);
    spec.cpus = args.cpus.unwrap_or(spec.cpus);
    spec.memory = args.mem.unwrap_or(spec.memory);
    spec.validate()?;
    source.check_arch(spec.arch)?;

    // Check the tools exist before spending minutes on a download.
    let backend = backend::get(&spec.backend)?;
    if let Some(c) = backend.checks().into_iter().chain(ssh::checks()).find(|c| !c.ok) {
        return Err(hinted(format!("{} not found", c.name), c.hint.unwrap_or_default()));
    }

    eprintln!(
        "  {} · {} · {} CPUs · {} RAM · {} disk",
        source.title(),
        spec.arch,
        spec.cpus,
        spec.memory,
        args.disk
    );
    let image = source.fetch(home, spec.arch)?;
    let client_key = ssh::client_key(home)?;
    spec.ssh.port = home.next_ssh_port()?;
    let vm = home.create(&args.name, spec, |vm| {
        backend.create(vm, &image, &args.disk)?;
        let host_key = ssh::host_key(vm)?;
        seed::write(vm, &client_key, &host_key)
    })?;
    let config = ssh::write_config(home, &vm, backend.ssh_addr(&vm))?;
    eprintln!("  ✓ disk  ✓ keys  ✓ cloud-init  ✓ ssh port {}", vm.spec.ssh.port);

    if args.no_start {
        eprintln!("  ✓ {} created; start it with `vx start {0}`", vm.name);
        return Ok(());
    }
    {
        let _lock = vm.lock()?;
        backend.start(&vm)?;
    }
    ssh::wait_ready(&config, &vm, backend, boot_timeout(&vm))?;
    ssh::wait_cloud_init(&config, &vm)?;
    eprintln!(
        "  ✓ {} is ready ({} s)    vx ssh {0}  ·  vx console {0}  ·  vx stop {0}",
        vm.name,
        started.elapsed().as_secs()
    );
    Ok(())
}

fn ssh(home: &Home, vm: &Vm, command: &[String]) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let config = ssh::write_config(home, vm, backend.ssh_addr(vm))?;
    if backend.state(vm) == State::Stopped {
        {
            let _lock = vm.lock()?;
            backend.start(vm)?;
        }
        ssh::wait_ready(&config, vm, backend, boot_timeout(vm))?;
    }
    ssh::exec(&config, vm, command)
}

/// Emulated guests boot many times slower.
fn boot_timeout(vm: &Vm) -> Duration {
    let native = Arch::host().is_ok_and(|a| a == vm.spec.arch);
    Duration::from_secs(if native { 180 } else { 900 })
}

fn images(home: &Home, prune: bool) -> Result<()> {
    if prune {
        let freed = image::prune(home)?;
        println!("removed {} of cached images", progress::bytes(freed));
        return Ok(());
    }
    let arch = Arch::host()?;
    let name_w = image::CATALOG.iter().map(|i| i.name.len()).max().unwrap_or(0);
    let title_w = image::CATALOG.iter().map(|i| i.title.len()).max().unwrap_or(0);
    println!("{:name_w$}  {:title_w$}  {:>8}  CACHED", "IMAGE", "DESCRIPTION", "DOWNLOAD");
    for img in image::CATALOG {
        let cached = if img.cached(home, arch).is_empty() { "-" } else { "yes" };
        let note = if img.name == image::DEFAULT {
            "(default)".to_string()
        } else if !img.supports(arch) {
            "(x86_64 only)".to_string()
        } else {
            String::new()
        };
        let size = format!("~{} MB", img.size_mb);
        let row = format!("{:name_w$}  {:title_w$}  {size:>8}  {cached:6}  {note}", img.name, img.title);
        println!("{}", row.trim_end());
    }
    println!();
    println!("vx new <name> --image <image>   or pass a path to a qcow2 file");
    println!(
        "cache: {} ({}); `vx images --prune` empties it",
        home.images().display(),
        progress::bytes(image::cache_size(home))
    );
    Ok(())
}
