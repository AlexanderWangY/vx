// Parts of these aren't used by any command yet.
#[allow(dead_code)]
mod backend;
mod console;
mod copy;
mod form;
#[allow(dead_code)]
mod host;
mod image;
mod mount;
mod ports;
mod progress;
#[allow(dead_code)]
mod qemu;
mod qmp;
mod seed;
mod set;
mod setup;
mod snapshot;
mod ssh;
mod stats;
mod style;
mod tui;
#[allow(dead_code)]
mod vx;

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use backend::State;
use host::Arch;
use image::Source;
use snapshot::History;
use style::{ERR, Label, OUT};
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
    /// Start a VM (leave out the name to pick one)
    Start { name: Option<String> },
    /// Stop a VM: power button first, then force it off
    Stop {
        name: Option<String>,
        /// Skip the power button and force it off right away
        #[arg(long)]
        force: bool,
    },
    /// Freeze a running VM in place
    Pause { name: Option<String> },
    /// Continue a paused VM
    Resume { name: Option<String> },
    /// SSH into a VM, starting it first if needed
    Ssh {
        name: Option<String>,
        /// A command to run instead of a shell
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Install packages in a VM; build-tools, python and a few others work on any distro
    Install {
        vm: String,
        #[arg(required = true, value_name = "PACKAGES")]
        packages: Vec<String>,
    },
    /// Copy files or directories into or out of a VM, starting it first if needed
    ///
    /// The VM's side is <vm>:<path>; relative paths start in your home there.
    ///   vx cp notes.txt src/ dev:/tmp/
    ///   vx cp dev:project/out.log .
    #[command(verbatim_doc_comment)]
    Cp {
        /// Sources, then the destination
        #[arg(required = true, num_args = 2.., value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Attach to a VM's serial console (Ctrl-] to detach)
    Console { name: Option<String> },
    /// Print a VM's boot log
    Logs {
        name: Option<String>,
        /// Keep printing new output
        #[arg(short, long)]
        follow: bool,
    },
    /// Change a VM's CPUs, memory or disk, or see them
    ///
    ///   vx set dev                    see them
    ///   vx set dev --cpus 8 --mem 16G
    ///   vx set dev --disk 40G         grows it, even while dev runs
    ///   vx set dev --disk +20G        20G more
    #[command(verbatim_doc_comment)]
    Set {
        /// The VM (leave it out to pick one)
        name: Option<String>,
        /// Virtual CPUs
        #[arg(long)]
        cpus: Option<u32>,
        /// Memory, e.g. 8G or 512M
        #[arg(long, value_parser = size)]
        mem: Option<String>,
        /// Disk size, e.g. 40G, or +20G for that much more; disks only grow
        #[arg(long, value_parser = set::parse_disk)]
        disk: Option<String>,
        /// Restart a running VM so new CPUs and memory take effect, without asking
        #[arg(long)]
        restart: bool,
    },
    /// Make a new VM that's a copy of another, or of one of its snapshots
    ///
    ///   vx clone dev             dev-2, a copy of dev as it is now
    ///   vx clone dev web         named web
    ///   vx clone dev@deps web    dev as it was at snapshot deps
    #[command(verbatim_doc_comment)]
    Clone {
        /// The VM to copy, or <vm>@<snapshot>
        #[arg(value_name = "VM[@SNAPSHOT]")]
        source: String,
        /// What to call the copy [default: <vm>-2, <vm>-3, …]
        name: Option<String>,
        /// Create it without starting it
        #[arg(long)]
        no_start: bool,
    },
    /// Rename a VM; a running one restarts to take its new name
    #[command(alias = "rename")]
    Mv {
        /// The VM to rename
        name: String,
        /// Its new name
        new_name: String,
        /// Restart it without asking, if it's running
        #[arg(short, long)]
        yes: bool,
    },
    /// Stop and delete a VM
    Rm {
        name: Option<String>,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Forward ports on this machine into a VM, or list them
    ///
    ///   vx port dev              list them
    ///   vx port dev 8080:80      localhost:8080 here reaches port 80 in dev
    ///   vx port dev 3000         the same port on both sides
    ///   vx port rm dev 8080      stop forwarding it
    #[command(verbatim_doc_comment)]
    Port(PortArgs),
    /// Share a folder on this machine with a VM, or list them
    ///
    ///   vx mount dev                  list them
    ///   vx mount dev ~/code           ~/code here is ~/code in dev too
    ///   vx mount dev .:/srv/app       this folder is /srv/app in dev
    ///   vx mount dev ~/notes --ro     dev can read it but not change it
    ///   vx mount rm dev ~/code        stop sharing it
    #[command(verbatim_doc_comment)]
    Mount(MountArgs),
    /// Keeps a running VM's mounts up; started by vx itself
    #[command(hide = true)]
    MountAgent { vm: String },
    /// Save a VM as it is now, to go back to later (`vx snap ls` shows them)
    Snap(SnapArgs),
    /// List the images `vx new` can use, or manage them
    Images {
        #[command(subcommand)]
        action: Option<ImagesCommand>,
    },
}

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
struct PortArgs {
    #[command(subcommand)]
    action: Option<PortCommand>,
    /// The VM (leave it out to pick one)
    vm: Option<String>,
    /// Forwards to add, as <port here>:<port in the VM>, or one port for both; none lists them
    #[arg(value_name = "PORTS")]
    ports: Vec<String>,
}

#[derive(Subcommand)]
enum PortCommand {
    /// Stop forwarding ports, by their number on this machine
    Rm {
        vm: String,
        #[arg(required = true)]
        ports: Vec<u16>,
    },
}

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
struct MountArgs {
    #[command(subcommand)]
    action: Option<MountCommand>,
    /// The VM (leave it out to pick one)
    vm: Option<String>,
    /// Folders to share, as <folder here>[:<place in the VM>]; none lists them
    #[arg(value_name = "FOLDERS")]
    folders: Vec<String>,
    /// Let the VM read them but not change them
    #[arg(long)]
    ro: bool,
}

#[derive(Subcommand)]
enum MountCommand {
    /// Stop sharing folders, by their path here or in the VM
    Rm {
        vm: String,
        #[arg(required = true, value_name = "FOLDERS")]
        folders: Vec<String>,
    },
}

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
struct SnapArgs {
    #[command(subcommand)]
    action: Option<SnapCommand>,
    /// The VM to snapshot (leave it out to pick one)
    vm: Option<String>,
    /// What to call the snapshot [default: snap-1, snap-2, …]
    name: Option<String>,
    /// A note to remember it by
    #[arg(short, long)]
    message: Option<String>,
}

#[derive(Subcommand)]
enum SnapCommand {
    /// List a VM's snapshots
    Ls { vm: Option<String> },
    /// Go back to a snapshot; the others are kept
    Restore {
        vm: String,
        snapshot: String,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete a snapshot; the ones taken after it stay
    Rm {
        vm: String,
        snapshot: String,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ImagesCommand {
    /// Download an image now, so `vx new` doesn't wait for it
    Pull {
        image: String,
        /// Download the build for this architecture instead of the host's
        #[arg(long)]
        arch: Option<Arch>,
    },
    /// Add a disk image of your own (qcow2 or raw) under a name
    Add { name: String, file: PathBuf },
    /// Delete an image's local copy: a cached download, or one you added
    Rm { image: String },
    /// Delete every cached download; images you added stay
    Prune,
}

#[derive(clap::Args)]
struct NewArgs {
    /// Leave it out to fill in a form instead
    name: Option<String>,
    /// Fill in a form, starting from the other flags
    #[arg(short, long)]
    interactive: bool,
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
    /// Packages to install, e.g. git,build-tools,python; added to the defaults in ~/.vx/config.toml
    #[arg(long, value_delimiter = ',', value_name = "PACKAGES")]
    install: Vec<String>,
    /// A script on this machine to run in the VM, as you, once the packages are in
    #[arg(long, value_name = "FILE")]
    setup: Option<String>,
    /// Skip the defaults in ~/.vx/config.toml
    #[arg(long)]
    bare: bool,
    /// Share a folder on this machine with it, as <folder>[:<place in the VM>]; repeatable
    #[arg(long, value_name = "FOLDER")]
    mount: Vec<String>,
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
        eprintln!("{} {e:#}", ERR.label(Label::Error, "error:"));
        if let Some(h) = e.chain().find_map(|c| c.downcast_ref::<Hinted>()) {
            eprintln!("{} {}", ERR.label(Label::Hint, "hint:"), h.hint);
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let home = Home::from_env()?;
    let Some(command) = cli.command else {
        // The dashboard needs a terminal; anywhere else, the plain list.
        return if form::interactive() { tui::run(&home) } else { ls(&home) };
    };
    // A missing VM name opens a picker; `None` means it was cancelled.
    let pick = |name, verb, question, prefer| pick(&home, name, verb, question, prefer);
    match command {
        Command::New(args) => {
            if let Some(args) = new_args(&home, args)? {
                new(&home, args)?;
            }
        }
        Command::Ls => ls(&home)?,
        Command::Start { name } => {
            if let Some(vm) = pick(name, "start", "Which VM do you want to start?", Some(State::Stopped))? {
                start(&vm)?;
            }
        }
        Command::Stop { name, force } => {
            if let Some(vm) = pick(name, "stop", "Which VM do you want to stop?", Some(State::Running))? {
                stop(&vm, force)?;
            }
        }
        Command::Pause { name } => {
            if let Some(vm) = pick(name, "pause", "Which VM do you want to pause?", Some(State::Running))? {
                pause(&vm, false)?;
            }
        }
        Command::Resume { name } => {
            if let Some(vm) = pick(name, "resume", "Which VM do you want to resume?", Some(State::Paused))? {
                pause(&vm, true)?;
            }
        }
        Command::Ssh { name, command } => {
            if let Some(vm) = pick(name, "ssh", "Which VM do you want to ssh into?", Some(State::Running))? {
                ssh(&home, &vm, &command)?;
            }
        }
        Command::Cp { paths } => cp(&home, &paths)?,
        Command::Install { vm, packages } => {
            let vm = home.load(&vm)?;
            let config = reachable(&home, &vm)?;
            setup::install(&config, &vm, &packages)?;
        }
        Command::Console { name } => {
            if let Some(vm) = pick(name, "console", "Which VM's console?", Some(State::Running))? {
                attach(&vm)?;
            }
        }
        Command::Logs { name, follow } => {
            if let Some(vm) = pick(name, "logs", "Which VM's boot log?", None)? {
                console::logs(&vm, follow)?;
            }
        }
        Command::Set { name, cpus, mem, disk, restart } => {
            if let Some(mut vm) = pick(name, "set", "Which VM do you want to change?", None)? {
                set::run(&home, &mut vm, set::Changes { cpus, memory: mem, disk }, restart)?;
            }
        }
        Command::Clone { source, name, no_start } => clone(&home, &source, name, no_start)?,
        Command::Mv { name, new_name, yes } => mv(&home, home.load(&name)?, &new_name, yes)?,
        Command::Rm { name, yes } => {
            if let Some(vm) = pick(name, "rm", "Which VM do you want to delete?", None)? {
                rm(vm, yes)?;
            }
        }
        Command::Port(PortArgs { action: None, vm, ports }) => {
            if let Some(mut vm) = pick(vm, "port", "Which VM's ports?", None)? {
                if ports.is_empty() {
                    ports::list(&vm)?;
                } else {
                    let forwards = ports.iter().map(|p| ports::parse(p)).collect::<Result<Vec<_>>>()?;
                    ports::add(&home, &mut vm, &forwards)?;
                }
            }
        }
        Command::Port(PortArgs { action: Some(PortCommand::Rm { vm, ports }), .. }) => {
            ports::remove(&mut home.load(&vm)?, &ports)?;
        }
        Command::Mount(MountArgs { action: None, vm, folders, ro }) => {
            if let Some(mut vm) = pick(vm, "mount", "Which VM's folders?", None)? {
                if folders.is_empty() {
                    mount::list(&vm)?;
                } else {
                    mount::check_host()?;
                    let mounts = folders.iter().map(|f| mount::parse(f, ro)).collect::<Result<Vec<_>>>()?;
                    mount::add(&home, &mut vm, mounts)?;
                }
            }
        }
        Command::Mount(MountArgs { action: Some(MountCommand::Rm { vm, folders }), .. }) => {
            mount::remove(&mut home.load(&vm)?, &folders)?;
        }
        Command::MountAgent { vm } => mount::agent(&home, &vm)?,
        Command::Snap(SnapArgs { action: None, vm, name, message }) => {
            if let Some(vm) = pick(vm, "snap", "Which VM do you want to snapshot?", None)? {
                snap(&home, &vm, name, message.unwrap_or_default())?;
            }
        }
        Command::Snap(SnapArgs { action: Some(SnapCommand::Ls { vm }), .. }) => {
            if let Some(vm) = pick(vm, "snap ls", "Whose snapshots?", None)? {
                snap_ls(&vm)?;
            }
        }
        Command::Snap(SnapArgs { action: Some(SnapCommand::Restore { vm, snapshot, yes }), .. }) => {
            snap_restore(&home.load(&vm)?, &snapshot, yes)?;
        }
        Command::Snap(SnapArgs { action: Some(SnapCommand::Rm { vm, snapshot, yes }), .. }) => {
            snap_rm(&home.load(&vm)?, &snapshot, yes)?;
        }
        Command::Images { action: None } => images(&home)?,
        Command::Images { action: Some(action) } => images_command(&home, action)?,
    }
    Ok(())
}

/// The VM called `name`, or one picked from a list when the name is left out.
fn pick(home: &Home, name: Option<String>, verb: &str, question: &str, prefer: Option<State>) -> Result<Option<Vm>> {
    let name = match name {
        Some(name) => name,
        None if form::interactive() => match form::pick_vm(home, verb, question, prefer)? {
            Some(name) => name,
            None => return Ok(None),
        },
        None => return Err(hinted("which VM?", format!("vx {verb} <name>; `vx ls` lists them"))),
    };
    home.load(&name).map(Some)
}

/// `vx new`'s arguments, from a form when the name is missing or `-i` is given.
fn new_args(home: &Home, args: NewArgs) -> Result<Option<NewArgs>> {
    if !args.interactive && args.name.is_some() {
        return Ok(Some(args));
    }
    if !form::interactive() {
        return Err(hinted("vx new needs a name here", "vx new <name>; the form only opens in a terminal"));
    }
    let Some(args) = form::new_vm(home, args)? else { return Ok(None) };
    eprintln!("  {}", ERR.dim(form::command_line(home, &args)?));
    Ok(Some(args))
}

fn ls(home: &Home) -> Result<()> {
    let names = home.names()?;
    if names.is_empty() {
        println!("no VMs yet; create one with `vx new <name>`");
        return Ok(());
    }
    let w = names.iter().map(String::len).max().unwrap_or(0).max("NAME".len());
    let header = format!("{:w$}  {:8}  {:7}  {:>4}  {:>6}  SSH", "NAME", "STATE", "ARCH", "CPUS", "MEMORY");
    println!("{}", OUT.dim(header));
    for tui::Entry { name, vm, .. } in tui::entries(home)? {
        match vm {
            Ok((s, state)) => {
                let state = match &state {
                    State::Running => OUT.green(state.to_string()),
                    State::Stopped => OUT.dim(state.to_string()),
                    // Paused, or something QEMU reported that needs a look (e.g. io-error).
                    _ => OUT.yellow(state.to_string()),
                };
                println!(
                    "{name:w$}  {state:8}  {:7}  {:>4}  {:>6}  127.0.0.1:{}",
                    s.arch, s.cpus, s.memory, s.ssh.port
                );
            }
            Err(e) => println!("{name:w$}  {} {e}", OUT.red("error:")),
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
    let tip = format!("· watch it boot with `vx console {0}` or `vx logs -f {0}`", vm.name);
    println!("{} {} started {}", OUT.green('✓'), vm.name, OUT.dim(tip));
    Ok(())
}

fn stop(vm: &Vm, force: bool) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let _lock = vm.lock()?;
    if backend.state(vm) == State::Stopped {
        println!("{}", OUT.dim(format!("{} is already stopped", vm.name)));
        return Ok(());
    }
    backend.stop(vm, force)?;
    println!("{} {} stopped", OUT.green('✓'), vm.name);
    Ok(())
}

/// Pause a running VM, or resume a paused one.
fn pause(vm: &Vm, resume: bool) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let Some(pause) = backend.pause() else {
        bail!("the {} backend can't pause VMs", vm.spec.backend);
    };
    let _lock = vm.lock()?;
    match (backend.state(vm), resume) {
        (State::Running, false) => pause.pause(vm)?,
        (State::Paused, true) => pause.resume(vm)?,
        (State::Paused, false) => bail!("{} is already paused", vm.name),
        (State::Running, true) => bail!("{} isn't paused", vm.name),
        (State::Stopped, _) => {
            return Err(hinted(format!("{} isn't running", vm.name), format!("vx start {}", vm.name)));
        }
        (State::Other(state), _) => bail!("{} is {state}", vm.name),
    }
    let done = if resume { "resumed" } else { "paused" };
    println!("{} {} {done}", OUT.green('✓'), vm.name);
    Ok(())
}

fn rm(vm: Vm, yes: bool) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    if !yes && !confirm(&vm)? {
        println!("kept {}", vm.name);
        return Ok(());
    }
    let _lock = vm.lock()?;
    // Its disk is about to go, so there's nothing to shut down gracefully.
    if backend.state(&vm) != State::Stopped {
        backend.stop(&vm, true)?;
    }
    mount::stop_agent(&vm);
    let name = vm.name.clone();
    vm.delete()?;
    println!("{} {name} deleted {}", OUT.green('✓'), OUT.dim("· cached images are kept"));
    Ok(())
}

/// `vx mv`: a new name for the VM, its SSH alias and, from its next boot, its hostname.
fn mv(home: &Home, vm: Vm, name: &str, yes: bool) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    if vm.name == name {
        println!("{}", OUT.dim(format!("{name} is called that already")));
        return Ok(());
    }
    home.check_new_name(name)?;
    let state = backend.state(&vm);
    if let State::Other(state) = &state {
        bail!("{} is {state}", vm.name);
    }
    // QEMU has the disk and its sockets open at their old paths, so it restarts around the move.
    let running = state != State::Stopped;
    if running && !yes {
        let question = format!("{} is {state}; restart it to rename it?", ERR.bold(&vm.name));
        let why = format!("not restarting {} without confirmation", vm.name);
        if !ask(&question, why, format!("vx mv -y {} {name}", vm.name))? {
            println!("left {} as it is", vm.name);
            return Ok(());
        }
    }
    let old = vm.name.clone();
    let vm = {
        let _lock = vm.lock()?;
        if running {
            backend.stop(&vm, false)?;
        }
        mount::stop_agent(&vm);
        let vm = home.rename(vm, name)?;
        // known_hosts pins the key under the VM's SSH alias, which is its name.
        let host_key = ssh::pin_host_key(&vm)?;
        // A new instance-id, so cloud-init sets the new hostname on its next boot, as for a
        // clone; the host key stays the same.
        seed::write(&vm, &ssh::client_key(home)?, &host_key)?;
        vm
    };
    let config = ssh::write_config(home, &vm, backend.ssh_addr(&vm))?;
    println!("{} {old} is now {name}", OUT.green('✓'));
    if running {
        {
            let _lock = vm.lock()?;
            backend.start(&vm)?;
        }
        ssh::wait_ready(&config, &vm, backend, boot_timeout(&vm))?;
        ssh::wait_cloud_init(&config, &vm)?; // which sets the new hostname
        println!("{} started {name} again {}", OUT.green('✓'), OUT.dim(format!("· vx ssh {name}")));
    } else {
        println!("{}", OUT.dim("its hostname changes too, when it next starts"));
    }
    Ok(())
}

fn confirm(vm: &Vm) -> Result<bool> {
    let question = format!("delete {} and its disk?", ERR.bold(&vm.name));
    ask(&question, format!("not deleting {} without confirmation", vm.name), format!("vx rm -y {}", vm.name))
}

/// Ask a yes/no question, defaulting to no. Without a terminal to ask on, fails with `why` and
/// `hint`, which should say how to skip the question.
fn ask(question: &str, why: String, hint: String) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Err(hinted(why, hint));
    }
    eprint!("{question} {} ", ERR.dim("[y/N]"));
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn snapshots(vm: &Vm) -> Result<&'static dyn backend::Snapshots> {
    let backend = backend::get(&vm.spec.backend)?;
    match backend.snapshots() {
        Some(s) => Ok(s),
        None => bail!("the {} backend can't take snapshots", vm.spec.backend),
    }
}

fn snap(home: &Home, vm: &Vm, name: Option<String>, note: String) -> Result<()> {
    let backend = snapshots(vm)?;
    let _lock = vm.lock()?;
    let mut history = History::load(vm, backend)?;
    let name = name.unwrap_or_else(|| history.next_name());
    snapshot::validate_name(&name)?;
    if history.get(&name).is_some() {
        return Err(hinted(
            format!("{} already has a snapshot called `{name}`", vm.name),
            format!("pick another name, or delete it with `vx snap rm {} {name}`", vm.name),
        ));
    }
    // Saving memory while the guest is still in its firmware or early boot leaves it hung
    // (QEMU with HVF), so memory waits until it's up, which sshd answering shows.
    let memory = match backend::get(&vm.spec.backend)?.state(vm) {
        State::Running => {
            let config = ssh::write_config(home, vm, backend::get(&vm.spec.backend)?.ssh_addr(vm))?;
            let up = ssh::reachable(&config, vm);
            if up {
                // So the disk alone is complete too, for `vx clone <vm>@<snapshot>`.
                ssh::sync(&config, vm);
            } else {
                style::warn("", format!("{} is still booting, so only its disk is saved", vm.name));
            }
            up
        }
        _ => true,
    };
    let saved = backend.save(vm, &name, memory)?;
    let what = match saved.memory {
        0 => "disk".to_string(),
        n => format!("memory and disk · {} of memory", progress::bytes(n)),
    };
    history.add(saved, note);
    history.save(vm)?;
    println!("{} saved {} as {name} {}", OUT.green('✓'), vm.name, OUT.dim(format!("({what})")));
    println!("{}", OUT.dim(format!("go back to it with `vx snap restore {} {name}`", vm.name)));
    Ok(())
}

fn snap_ls(vm: &Vm) -> Result<()> {
    let backend = snapshots(vm)?;
    let history = History::load(vm, backend)?;
    if history.entries.is_empty() {
        println!("{} has no snapshots yet; take one with `vx snap {}`", vm.name, vm.name);
        return Ok(());
    }
    let name_w = history.entries.iter().map(|e| e.snap.name.chars().count()).max().unwrap_or(0);
    for (i, e) in history.entries.iter().enumerate() {
        let marker = snapshot::marker(e);
        let marker = if e.snap.memory > 0 { OUT.green(marker).to_string() } else { OUT.blue(marker).to_string() };
        let here = history.current.as_deref() == Some(e.snap.name.as_str());
        let label = format!(" {} ", e.snap.name);
        let pad = " ".repeat(name_w - e.snap.name.chars().count());
        let name = if here { format!("{}{pad}", OUT.label_here(label)) } else { format!("{label}{pad}") };
        let memory = match e.snap.memory {
            0 => "disk only".to_string(),
            n => format!("{} memory", progress::bytes(n)),
        };
        let when = format!("{:>8}", snapshot::ago(e.snap.created));
        // Where it was saved from, when that isn't the line above: after going back.
        let note = match (history.from(i), e.note.as_str()) {
            (Some(from), "") => OUT.dim(format!("from {from}")).to_string(),
            (Some(from), note) => format!("{} {note}", OUT.dim(format!("from {from} ·"))),
            (None, note) => note.to_string(),
        };
        let line = format!("{marker}{name} {}  {}  {note}", OUT.dim(when), OUT.dim(format!("{memory:>13}")));
        println!("{}", line.trim_end());
    }
    println!();
    let state = backend::get(&vm.spec.backend)?.state(vm);
    match &history.current {
        Some(at) => {
            println!(
                "current snapshot: {} {}",
                OUT.label_here(format!(" {at} ")),
                OUT.dim(format!("· {} is {state}", vm.name))
            )
        }
        None => println!("no current snapshot {}", OUT.dim(format!("· {} is {state}", vm.name))),
    }
    println!(
        "{} {}   {} {}",
        OUT.green('●'),
        OUT.dim("resumes running: memory saved"),
        OUT.blue('○'),
        OUT.dim("boots from disk: disk only")
    );
    Ok(())
}

fn snap_restore(vm: &Vm, name: &str, yes: bool) -> Result<()> {
    let backend = snapshots(vm)?;
    let mut history = History::load(vm, backend)?;
    let entry = history.find(vm, name)?.clone();
    if !yes {
        let question = format!(
            "go back to {} in {}? what it has now is lost unless you snapshot it first; other snapshots are kept",
            ERR.bold(name),
            ERR.bold(&vm.name),
        );
        let why = format!("not restoring {} without confirmation", vm.name);
        if !ask(&question, why, format!("vx snap restore -y {} {name}", vm.name))? {
            println!("left {} as it is", vm.name);
            return Ok(());
        }
    }
    let _lock = vm.lock()?;
    backend.restore(vm, &entry.snap)?;
    history.current = Some(name.to_string());
    history.save(vm)?;
    let state = backend::get(&vm.spec.backend)?.state(vm);
    println!("{} {} is back at {name} {}", OUT.green('✓'), vm.name, OUT.dim(format!("({state})")));
    Ok(())
}

fn snap_rm(vm: &Vm, name: &str, yes: bool) -> Result<()> {
    let backend = snapshots(vm)?;
    let mut history = History::load(vm, backend)?;
    history.find(vm, name)?;
    if !yes {
        let question = format!("delete snapshot {} of {}?", ERR.bold(name), ERR.bold(&vm.name));
        let why = format!("not deleting {name} without confirmation");
        if !ask(&question, why, format!("vx snap rm -y {} {name}", vm.name))? {
            println!("kept {name}");
            return Ok(());
        }
    }
    let _lock = vm.lock()?;
    backend.delete(vm, name)?;
    history.remove(name);
    history.save(vm)?;
    println!("{} deleted {name} from {}", OUT.green('✓'), vm.name);
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
    let name = args.name.clone().unwrap_or_default();
    home.check_new_name(&name)?;
    let source = Source::parse(home, &args.image)?;
    let mut spec = Spec::defaults()?;
    spec.image = source.name();
    spec.arch = args.arch.unwrap_or(spec.arch);
    spec.cpus = args.cpus.unwrap_or(spec.cpus);
    spec.memory = args.mem.unwrap_or(spec.memory);
    spec.validate()?;
    source.check_arch(spec.arch)?;
    let defaults = setup::Config::load(home)?;
    let plan = setup::Plan::new(&defaults.new, args.bare, &args.install, args.setup.as_deref());
    plan.check()?;
    let mounts = args.mount.iter().map(|m| mount::parse(m, false)).collect::<Result<Vec<_>>>()?;
    if !mounts.is_empty() {
        mount::check_host()?;
    }

    // Check the tools exist before spending minutes on a download.
    let backend = backend::get(&spec.backend)?;
    if let Some(c) = backend.checks().into_iter().chain(ssh::checks()).find(|c| !c.ok) {
        return Err(hinted(format!("{} not found", c.name), c.hint.unwrap_or_default()));
    }

    let dot = ERR.dim(" · ");
    eprintln!(
        "  {}{dot}{}{dot}{} CPUs{dot}{} RAM{dot}{} disk",
        ERR.bold(source.title()),
        spec.arch,
        spec.cpus,
        spec.memory,
        args.disk
    );
    let image = source.fetch(home, spec.arch)?;
    let client_key = ssh::client_key(home)?;
    spec.ssh.port = home.next_ssh_port()?;
    let vm = home.create(&name, spec, |vm| {
        backend.create(vm, &image, &args.disk)?;
        let host_key = ssh::host_key(vm)?;
        seed::write(vm, &client_key, &host_key)
    })?;
    let config = ssh::write_config(home, &vm, backend.ssh_addr(&vm))?;
    let check = ERR.green('✓');
    eprintln!("  {check} disk  {check} keys  {check} cloud-init  {check} ssh port {}", vm.spec.ssh.port);

    if args.no_start {
        // Nothing runs yet, so they can go straight in; they're mounted when it starts.
        let mut vm = vm;
        vm.spec.mounts = mounts;
        vm.save()?;
        let tip = format!("· start it with `vx start {}`", vm.name);
        eprintln!("  {check} {} created {}", ERR.bold(&vm.name), ERR.dim(tip));
        if !plan.install.is_empty() {
            let install = format!("vx install {} {}", vm.name, plan.install.join(" "));
            style::warn("  ", format!("nothing was installed, since it didn't start; once it has: {install}"));
        }
        return Ok(());
    }
    first_boot(&vm, backend, &config)?;
    if !plan.install.is_empty() {
        setup::install(&config, &vm, &plan.install)?;
    }
    // After the packages, so the mount agent's own install of sshfs doesn't wait on theirs.
    let vm = if mounts.is_empty() {
        vm
    } else {
        let mut vm = vm;
        vm.spec.mounts = mounts;
        vm.save()?;
        mount::ensure_agent(&vm)?;
        wait_for_mounts(&vm);
        vm
    };
    if let Some(script) = &plan.setup {
        setup::run_script(&config, &vm, script)?;
    }
    ready(&vm, started);
    // Asked for by flag with no defaults yet: show how to stop typing them every time.
    if !args.install.is_empty() && defaults.new.install.is_empty() && !args.bare {
        eprintln!("  {}", ERR.dim(setup::tip(&args.install)));
    }
    Ok(())
}

/// Start a VM that's never run, and wait for it to accept logins and for cloud-init to set it up.
fn first_boot(vm: &Vm, backend: &dyn backend::Backend, config: &Path) -> Result<()> {
    {
        let _lock = vm.lock()?;
        backend.start(vm)?;
    }
    ssh::wait_ready(config, vm, backend, boot_timeout(vm))?;
    ssh::wait_cloud_init(config, vm)
}

/// Wait for a new VM's folders to be mounted, and say which are.
fn wait_for_mounts(vm: &Vm) {
    let failed = mount::wait(vm, |_| true, Duration::from_secs(60));
    let ok: Vec<&str> =
        vm.spec.mounts.iter().filter(|m| !failed.iter().any(|(f, _)| f == *m)).map(|m| m.guest.as_str()).collect();
    if !ok.is_empty() {
        eprintln!("  {} mounted {}", ERR.green('✓'), ok.join(", "));
    }
    mount::report(vm, &failed);
}

fn ready(vm: &Vm, started: Instant) {
    let took = format!("({} s)", started.elapsed().as_secs());
    let dot = ERR.dim("  ·  ");
    let name = &vm.name;
    eprintln!(
        "  {} {} is ready {}    vx ssh {name}{dot}vx console {name}{dot}vx stop {name}",
        ERR.green('✓'),
        ERR.bold(name),
        ERR.dim(took)
    );
}

/// `vx clone`: a new VM whose disk is a copy of `source`'s (`dev`, or `dev@snapshot` for its
/// disk as it was then), with an identity of its own: name, hostname, SSH port and host key.
fn clone(home: &Home, source: &str, name: Option<String>, no_start: bool) -> Result<()> {
    let started = Instant::now();
    let (from_name, snap) = match source.split_once('@') {
        Some((vm, snap)) => (vm, Some(snap)),
        None => (source, None),
    };
    let from = home.load(from_name)?;
    let backend = backend::get(&from.spec.backend)?;
    let Some(clones) = backend.clones() else {
        bail!("the {} backend can't clone VMs", from.spec.backend);
    };
    let memory = match snap {
        Some(snap) => History::load(&from, snapshots(&from)?)?.find(&from, snap)?.snap.memory > 0,
        None => false,
    };
    let name = name.unwrap_or_else(|| home.clone_name(&from.name));
    home.check_new_name(&name)?;
    let state = backend.state(&from);
    if let State::Other(state) = &state {
        bail!("{} is {state}", from.name);
    }
    let mut spec = from.spec.clone();
    // They'd clash with the source's, which keeps them.
    let ports = std::mem::take(&mut spec.forward);
    spec.ssh.port = home.next_ssh_port()?;
    let client_key = ssh::client_key(home)?;

    let dot = ERR.dim(" · ");
    let what = match snap {
        Some(snap) => format!("{} at {snap}", from.name),
        None => from.name.clone(),
    };
    let note = match (snap, &state) {
        (Some(_), _) if memory => "its disk; it boots fresh",
        (None, State::Running) => "its disk as it is now; it keeps running",
        _ => "",
    };
    eprintln!(
        "  {}{}",
        ERR.bold(format!("{what} → {name}")),
        if note.is_empty() { String::new() } else { format!("{dot}{}", ERR.dim(note)) }
    );
    let vm = {
        let _source = from.lock()?;
        if snap.is_none() && state == State::Running {
            // Gets what the guest has written but not yet put on disk into the copy.
            ssh::sync(&ssh::write_config(home, &from, backend.ssh_addr(&from))?, &from);
        }
        home.create(&name, spec, |vm| {
            clones.clone_disk(&from, snap, vm)?;
            // A new host key, and a new instance-id, so cloud-init sets it up as a machine of
            // its own on first boot: its hostname, and the key.
            let host_key = ssh::host_key(vm)?;
            seed::write(vm, &client_key, &host_key)
        })?
    };
    let config = ssh::write_config(home, &vm, backend.ssh_addr(&vm))?;
    let check = ERR.green('✓');
    eprintln!("  {check} disk  {check} keys  {check} cloud-init  {check} ssh port {}", vm.spec.ssh.port);
    if !ports.is_empty() {
        let tip = format!("forward others with `vx port {} …`", vm.name);
        eprintln!("  {}", ERR.dim(format!("{}'s forwarded ports stay with it; {tip}", from.name)));
    }
    if no_start {
        let tip = format!("· start it with `vx start {}`", vm.name);
        eprintln!("  {check} {} created {}", ERR.bold(&vm.name), ERR.dim(tip));
        return Ok(());
    }
    first_boot(&vm, backend, &config)?;
    if !vm.spec.mounts.is_empty() {
        wait_for_mounts(&vm);
    }
    ready(&vm, started);
    Ok(())
}

fn ssh(home: &Home, vm: &Vm, command: &[String]) -> Result<()> {
    let config = reachable(home, vm)?;
    ssh::exec(&config, vm, command)
}

fn cp(home: &Home, paths: &[String]) -> Result<()> {
    let names = home.names()?;
    let places = paths.iter().map(|p| copy::Place::parse(p, |n| names.iter().any(|v| v == n)));
    let plan = copy::plan(places.collect::<Result<_>>()?)?;
    let vm = home.load(&plan.vm)?;
    let config = reachable(home, &vm)?;
    ssh::copy(&config, &plan.operands)
}

/// Write the VM's ssh_config and return it, starting the VM first if it's stopped.
fn reachable(home: &Home, vm: &Vm) -> Result<PathBuf> {
    let backend = backend::get(&vm.spec.backend)?;
    let config = ssh::write_config(home, vm, backend.ssh_addr(vm))?;
    match backend.state(vm) {
        State::Stopped => {
            {
                let _lock = vm.lock()?;
                backend.start(vm)?;
            }
            ssh::wait_ready(&config, vm, backend, boot_timeout(vm))?;
        }
        // A paused VM accepts the connection but never answers, so ssh would just hang.
        State::Paused => {
            return Err(hinted(format!("{} is paused", vm.name), format!("vx resume {}", vm.name)));
        }
        _ => {}
    }
    // So a shared folder is there to use as soon as you're in.
    if !vm.spec.mounts.is_empty() {
        mount::ensure_agent(vm)?;
        mount::report(vm, &mount::wait(vm, |_| true, Duration::from_secs(30)));
    }
    Ok(config)
}

/// Emulated guests boot many times slower.
fn boot_timeout(vm: &Vm) -> Duration {
    let native = Arch::host().is_ok_and(|a| a == vm.spec.arch);
    Duration::from_secs(if native { 180 } else { 900 })
}

fn images(home: &Home) -> Result<()> {
    let arch = Arch::host()?;
    let infos = image::infos(home, arch);
    let name_w = infos.iter().map(|i| i.name.len()).max().unwrap_or(0);
    let title_w = infos.iter().map(|i| i.title.len()).max().unwrap_or(0);
    let header = format!("{:name_w$}  {:title_w$}  {:>8}  ON DISK", "IMAGE", "DESCRIPTION", "SIZE");
    println!("{}", OUT.dim(header));
    for info in &infos {
        let size = if info.custom { progress::bytes(info.size) } else { format!("~{}", progress::bytes(info.size)) };
        let on_disk = match info.downloading {
            Some(done) => OUT.cyan(format!("↓ {}", progress::bytes(done))),
            None if info.on_disk > 0 => OUT.green(progress::bytes(info.on_disk)),
            None => OUT.dim("-".to_string()),
        };
        let row = format!("{:name_w$}  {:title_w$}  {size:>8}  {on_disk:7}", info.name, info.title);
        if info.name == image::DEFAULT {
            println!("{row}  {}", OUT.dim("default"));
        } else if info.custom {
            println!("{row}  {}", OUT.cyan("custom"));
        } else if !info.native {
            let other = if arch == Arch::Aarch64 { Arch::X86_64 } else { Arch::Aarch64 };
            println!("{row}  {}", OUT.yellow(format!("{other} only")));
        } else {
            println!("{}", row.trim_end());
        }
    }
    println!();
    println!("{}", OUT.dim("vx new <name> --image <image> · vx images pull | add | rm | prune"));
    let cache = format!("downloads: {} in {}", progress::bytes(image::cache_size(home)), home.images().display());
    println!("{}", OUT.dim(cache));
    Ok(())
}

fn images_command(home: &Home, action: ImagesCommand) -> Result<()> {
    match action {
        ImagesCommand::Pull { image, arch } => {
            let Some(found) = image::find(&image) else {
                return Err(hinted(format!("no image called `{image}` to download"), "`vx images` lists them"));
            };
            found.fetch(home, arch.map_or_else(Arch::host, Ok)?)?;
        }
        ImagesCommand::Add { name, file } => {
            eprintln!("{}", ERR.dim(format!("copying {}…", file.display())));
            let path = image::add_custom(home, &name, &file)?;
            let size = progress::bytes(path.metadata()?.len());
            println!("{} added {name} {}", OUT.green('✓'), OUT.dim(format!("({size}) · vx new <name> --image {name}")));
        }
        ImagesCommand::Rm { image } => {
            let freed = image::remove(home, &image)?;
            println!("{} deleted {image} {}", OUT.green('✓'), OUT.dim(format!("({} freed)", progress::bytes(freed))));
        }
        ImagesCommand::Prune => {
            let freed = image::prune(home)?;
            println!("{} removed {} of downloads", OUT.green('✓'), progress::bytes(freed));
        }
    }
    Ok(())
}
