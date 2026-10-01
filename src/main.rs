// Not wired into the commands yet.
#[allow(dead_code)]
mod backend;
#[allow(dead_code)]
mod qemu;
#[allow(dead_code)]
mod vx;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    /// Stop a VM
    Stop { name: String },
    /// SSH into a VM
    Ssh { name: String },
    /// Stop and delete a VM
    Rm { name: String },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => println!("dashboard"),
        Some(Command::New { name }) => println!("new {name}"),
        Some(Command::Ls) => println!("ls"),
        Some(Command::Start { name }) => println!("start {name}"),
        Some(Command::Stop { name }) => println!("stop {name}"),
        Some(Command::Ssh { name }) => println!("ssh {name}"),
        Some(Command::Rm { name }) => println!("rm {name}"),
    }
    Ok(())
}
