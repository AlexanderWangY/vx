//! `vx port`: forward TCP ports on this machine (127.0.0.1 only) into a VM. vx.toml's `forward`
//! list holds them, so they come back each time the VM starts, and a running VM picks up
//! changes straight away.

use std::net::{Ipv4Addr, TcpListener};

use anyhow::{Result, bail};

use crate::backend::{self, State};
use crate::hinted;
use crate::style::OUT;
use crate::vx::{Home, Vm};

/// `8080:80` forwards 8080 here to 80 in the VM; `3000` is `3000:3000`.
pub fn parse(arg: &str) -> Result<(u16, u16)> {
    let port = |p: &str| p.parse::<u16>().ok().filter(|&p| p != 0);
    let pair = match arg.split_once(':') {
        Some((host, guest)) => port(host).zip(port(guest)),
        None => port(arg).map(|p| (p, p)),
    };
    pair.ok_or_else(|| {
        hinted(
            format!("`{arg}` isn't a port forward"),
            "write it as <port here>:<port in the VM>, e.g. 8080:80, or one port for both, e.g. 3000",
        )
    })
}

/// Why `host` can't be forwarded to `vm`, if it can't: it's the VM's SSH port, another VM
/// forwards it, or something on this machine is listening on it.
fn check_free(home: &Home, vm: &Vm, host: u16) -> Result<()> {
    if host == vm.spec.ssh.port {
        bail!("port {host} is {}'s SSH port", vm.name);
    }
    for name in home.names()? {
        if name == vm.name {
            continue;
        }
        if let Ok(other) = home.load(&name)
            && other.spec.host_ports().any(|p| p == host)
        {
            return Err(hinted(
                format!("{name} already forwards port {host}"),
                format!("pick another port here, e.g. {}:…", host.saturating_add(1)),
            ));
        }
    }
    if TcpListener::bind((Ipv4Addr::LOCALHOST, host)).is_err() {
        let why = if host < 1024 && cfg!(target_os = "linux") {
            "needs root on Linux, or something is using it"
        } else {
            "is in use by something on this machine"
        };
        return Err(hinted(format!("port {host} {why}"), format!("pick another, e.g. {}:…", host.saturating_add(1))));
    }
    Ok(())
}

pub fn add(home: &Home, vm: &mut Vm, forwards: &[(u16, u16)]) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let _lock = vm.lock()?;
    let running = backend.state(vm) != State::Stopped;
    for &(host, guest) in forwards {
        match vm.spec.forwards().find(|(h, _)| *h == host) {
            Some((_, g)) if g == guest => {
                println!("{}", OUT.dim(format!("{} already forwards {host} to {guest}", vm.name)));
                continue;
            }
            Some((_, g)) => {
                return Err(hinted(
                    format!("{} already forwards port {host}, to {g}", vm.name),
                    format!("remove it first with `vx port rm {} {host}`", vm.name),
                ));
            }
            None => {}
        }
        check_free(home, vm, host)?;
        if running {
            let Some(live) = backend.forwards() else {
                bail!("the {} backend can't add forwards to a running VM; stop it first", vm.spec.backend);
            };
            live.add(vm, host, guest)?;
        }
        vm.spec.forward.push(format!("{host}:{guest}"));
        vm.save()?;
        let when = if running { "" } else { " (from when it starts)" };
        println!("{} localhost:{host} → {}:{guest}{}", OUT.green('✓'), vm.name, OUT.dim(when));
    }
    Ok(())
}

pub fn remove(vm: &mut Vm, hosts: &[u16]) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let _lock = vm.lock()?;
    let running = backend.state(vm) != State::Stopped;
    for &host in hosts {
        let Some(i) = vm.spec.forwards().position(|(h, _)| h == host) else {
            if host == vm.spec.ssh.port {
                bail!("port {host} is {}'s SSH port, which it always forwards", vm.name);
            }
            return Err(hinted(format!("{} doesn't forward port {host}", vm.name), format!("vx port {}", vm.name)));
        };
        if running && let Some(live) = backend.forwards() {
            // Already gone if vx.toml was edited by hand since the VM started.
            if live.active(vm)?.iter().any(|(h, _)| *h == host) {
                live.remove(vm, host)?;
            }
        }
        vm.spec.forward.remove(i);
        vm.save()?;
        println!("{} stopped forwarding localhost:{host}", OUT.green('✓'));
    }
    Ok(())
}

pub fn list(vm: &Vm) -> Result<()> {
    let backend = backend::get(&vm.spec.backend)?;
    let state = backend.state(vm);
    // What the running VM actually does, to point out where vx.toml has changed since it started.
    let active = match (&state, backend.forwards()) {
        (State::Stopped, _) | (_, None) => None,
        (_, Some(live)) => live.active(vm).ok(),
    };
    let ssh = (vm.spec.ssh.port, 22);
    let rows: Vec<(u16, u16, &str)> =
        std::iter::once((ssh.0, ssh.1, "ssh")).chain(vm.spec.forwards().map(|(h, g)| (h, g, ""))).collect();
    let w = rows.iter().map(|(h, _, _)| h.to_string().len()).max().unwrap_or(4);
    for (host, guest, what) in &rows {
        let missing = active.as_ref().is_some_and(|a| !a.contains(&(*host, *guest)));
        let note = if missing {
            OUT.yellow(format!("not active: added to vx.toml after {} started; restart it", vm.name)).to_string()
        } else {
            OUT.dim(what.to_string()).to_string()
        };
        let line = format!("localhost:{host:<w$} → {}:{guest:<5} {note}", vm.name);
        println!("{}", line.trim_end());
    }
    if let Some(active) = &active {
        for (host, guest) in active.iter().filter(|f| !rows.iter().any(|(h, g, _)| (*h, *g) == **f)) {
            let note = OUT.yellow("active until it stops: not in vx.toml any more");
            println!("localhost:{host:<w$} → {}:{guest:<5} {note}", vm.name);
        }
    }
    if rows.len() == 1 {
        println!();
        println!("{}", OUT.dim(format!("forward one with `vx port {} 8080:80`", vm.name)));
    } else if state == State::Stopped {
        println!();
        println!("{}", OUT.dim(format!("{} is stopped; these start with it", vm.name)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forwards() {
        assert_eq!(parse("8080:80").unwrap(), (8080, 80));
        assert_eq!(parse("3000").unwrap(), (3000, 3000));
        for bad in ["", "0", "80:0", "http", "8080:", ":80", "70000", "1:2:3"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
