//! `vx set`: change a VM's CPUs, memory and disk after it's made.
//!
//! CPUs and memory are vx.toml settings, so they take effect when the VM next starts; `vx set`
//! offers to restart a running VM for them. A disk grows straight away, even while the VM runs,
//! and so does the filesystem on it: cloud-init's growpart and resizefs, which every built-in
//! image runs at boot anyway, run once more.

use std::io::{self, IsTerminal};
use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::backend::{self, State};
use crate::style::{self, ERR, OUT};
use crate::vx::{Home, Vm, show_size, size_bytes};
use crate::{hinted, host, ssh};

/// What `vx set` was asked to change; `None` leaves it as it is.
#[derive(Debug, Default)]
pub struct Changes {
    pub cpus: Option<u32>,
    pub memory: Option<String>,
    /// A size, or `+` and how much to add.
    pub disk: Option<String>,
}

impl Changes {
    fn is_empty(&self) -> bool {
        self.cpus.is_none() && self.memory.is_none() && self.disk.is_none()
    }
}

/// As many as this machine has.
pub fn max_cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get()) as u32
}

/// The size `asked` (`40G`, or `+20G` on top of `now`) makes a disk, in bytes.
pub fn disk_target(now: u64, asked: &str) -> Option<u64> {
    match asked.strip_prefix('+') {
        Some(more) => size_bytes(more).and_then(|more| now.checked_add(more)),
        None => size_bytes(asked),
    }
}

/// `40G`, or `+20G` for that much more.
pub fn parse_disk(s: &str) -> Result<String, String> {
    if size_bytes(s.strip_prefix('+').unwrap_or(s)).is_some() {
        Ok(s.into())
    } else {
        Err("use a size like 40G, or +20G for that much more".into())
    }
}

pub fn run(home: &Home, vm: &mut Vm, changes: Changes, restart: bool) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    if changes.is_empty() && !restart {
        return show(vm);
    }
    let state = backend.state(vm);
    if let State::Other(state) = &state {
        bail!("{} is {state}", vm.name);
    }

    // Everything is checked before anything changes.
    if let Some(cpus) = changes.cpus {
        let host = max_cpus();
        if cpus == 0 || cpus > host {
            return Err(hinted(
                format!("{} can have 1 to {host} CPUs, as many as this machine has", vm.name),
                format!("vx set {} --cpus {}", vm.name, host.min(cpus.max(1))),
            ));
        }
    }
    if let Some(memory) = &changes.memory
        && let (Some(want), Some(host)) = (size_bytes(memory), host::memory())
        && want > host
    {
        return Err(hinted(
            format!("this machine has {} of memory, less than {memory}", show_size(host)),
            format!("vx set {} --mem {}", vm.name, show_size((host / 2) >> 30 << 30)),
        ));
    }
    let disk = match &changes.disk {
        None => None,
        Some(asked) => {
            let Some(disks) = backend.disks() else {
                bail!("the {} backend can't resize disks", vm.spec.backend);
            };
            let now = disks.disk_size(vm)?;
            let want = disk_target(now, asked).context("disk size is too big")?;
            if want < now {
                return Err(hinted(
                    format!("disks can only grow, and {}'s is {} already", vm.name, show_size(now)),
                    format!("vx set {} --disk +10G, for 10G more", vm.name),
                ));
            }
            (want > now).then_some((disks, now, want))
        }
    };

    let mut changed = Vec::new();
    {
        let _lock = vm.lock()?;
        if let Some(cpus) = changes.cpus.filter(|c| *c != vm.spec.cpus) {
            changed.push(format!("{cpus} CPUs {}", OUT.dim(format!("(was {})", vm.spec.cpus))));
            vm.spec.cpus = cpus;
        }
        if let Some(memory) = changes.memory.filter(|m| size_bytes(m) != size_bytes(&vm.spec.memory)) {
            let memory = show_size(size_bytes(&memory).unwrap_or_default());
            changed.push(format!("{memory} memory {}", OUT.dim(format!("(was {})", vm.spec.memory))));
            vm.spec.memory = memory;
        }
        vm.save()?;
        if let Some((disks, now, want)) = disk {
            disks.grow_disk(vm, want)?;
            changed.push(format!("{} disk {}", show_size(want), OUT.dim(format!("(was {})", show_size(now)))));
        }
    }
    // Changed now, or by an earlier `vx set` that didn't restart it.
    let stale = state != State::Stopped && behind(vm).is_some();
    if changed.is_empty() && !stale {
        println!("{}", OUT.dim(format!("{} already has those", vm.name)));
        return Ok(());
    }
    if !changed.is_empty() {
        println!("{} {}: {}", OUT.green('✓'), vm.name, changed.join(&OUT.dim(" · ").to_string()));
    }

    if disk.is_some() {
        match state {
            State::Running if grow_filesystem(home, vm) => {
                println!("{} its filesystem has the room now", OUT.green('✓'));
            }
            State::Running => style::warn(
                "",
                format!("{0}'s filesystem didn't grow; it will next time {0} starts, or restart it now", vm.name),
            ),
            _ => println!("{}", OUT.dim("its filesystem grows into the room the next time it starts")),
        }
    }
    if stale {
        let ask = || {
            io::stdin().is_terminal()
                && crate::ask(
                    &format!("restart {} now, so its CPUs and memory change?", ERR.bold(&vm.name)),
                    String::new(),
                    String::new(),
                )
                .unwrap_or(false)
        };
        if restart || ask() {
            {
                let _lock = vm.lock()?;
                if backend.state(vm) != State::Stopped {
                    backend.stop(vm, false)?;
                }
                backend.start(vm)?;
            }
            // So it's ready to use when this returns, as it was before.
            let config = ssh::write_config(home, vm, backend.ssh_addr(vm))?;
            ssh::wait_ready(&config, vm, backend, crate::boot_timeout(vm))?;
            let now = format!("{} CPUs · {} memory", vm.spec.cpus, vm.spec.memory);
            println!("{} restarted {} {}", OUT.green('✓'), vm.name, OUT.dim(format!("with {now}")));
        } else {
            let already =
                if changed.is_empty() { format!("{} is set to that already; ", vm.name) } else { String::new() };
            let tip = format!("{already}CPUs and memory change when it restarts: `vx set {} --restart`", vm.name);
            println!("{}", OUT.dim(tip));
        }
    }
    Ok(())
}

/// What a running VM has, as `2 CPUs · 2G memory`, when it's not what vx.toml says.
fn behind(vm: &Vm) -> Option<String> {
    let (cpus, memory) = backend::get(&vm.spec.backend).ok()?.running_size(vm)?;
    let same = cpus == vm.spec.cpus && Some(memory) == size_bytes(&vm.spec.memory);
    (!same).then(|| format!("{cpus} CPUs · {} memory", show_size(memory)))
}

/// Grow the partition and filesystem of a running VM into its disk's new room. Whether it did.
fn grow_filesystem(home: &Home, vm: &Vm) -> bool {
    let Ok(backend) = backend::get(&vm.spec.backend) else { return false };
    let Ok(config) = ssh::write_config(home, vm, backend.ssh_addr(vm)) else { return false };
    let grow = "sudo -n cloud-init single --name growpart && sudo -n cloud-init single --name resizefs";
    ssh::command(&config, vm)
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", grow])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `vx set dev` on its own: what it has now, and how to change it.
fn show(vm: &Vm) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let disk = backend.disks().and_then(|d| d.disk_size(vm).ok()).map(show_size);
    let dot = OUT.dim(" · ");
    let mut line = format!("{}: {} CPUs{dot}{} memory", vm.name, vm.spec.cpus, vm.spec.memory);
    if let Some(disk) = disk {
        line += &format!("{dot}{disk} disk");
    }
    println!("{line}");
    if let Some(running) = behind(vm) {
        println!("{}", OUT.yellow(format!("running with {running} until it restarts")));
    }
    println!();
    println!("{}", OUT.dim(format!("change them with e.g. `vx set {} --cpus 8 --mem 8G --disk +20G`", vm.name)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_sizes() {
        for ok in ["40G", "+20G", "512M", "+1T"] {
            assert!(parse_disk(ok).is_ok(), "{ok}");
        }
        for bad in ["", "+", "big", "-5G", "++5G", "40GB"] {
            assert!(parse_disk(bad).is_err(), "{bad}");
        }
    }
}
