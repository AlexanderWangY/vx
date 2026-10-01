// Parts of these aren't used by any command yet.
#[allow(dead_code)]
mod backend;
mod console;
#[allow(dead_code)]
mod host;
#[allow(dead_code)]
mod qemu;
mod qmp;
#[allow(dead_code)]
mod vx;

use std::fmt;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use backend::State;
use vx::{Home, Vm};

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
    New { name: String },
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
    /// SSH into a VM
    Ssh { name: String },
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
        Command::New { name } => println!("new {name}"),
        Command::Ls => ls(&home)?,
        Command::Start { name } => start(&home.load(&name)?)?,
        Command::Stop { name, force } => stop(&home.load(&name)?, force)?,
        Command::Ssh { name } => println!("ssh {name}"),
        Command::Console { name } => attach(&home.load(&name)?)?,
        Command::Logs { name, follow } => console::logs(&home.load(&name)?, follow)?,
        Command::Rm { name } => println!("rm {name}"),
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
