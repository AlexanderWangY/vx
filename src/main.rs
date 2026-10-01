// Not wired into the commands yet.
#[allow(dead_code)]
mod backend;
#[allow(dead_code)]
mod qemu;
#[allow(dead_code)]
mod vx;

use std::fmt;

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
