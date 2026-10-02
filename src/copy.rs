//! `vx cp`: copy files and directories between this machine and a VM, the way `scp` does,
//! with `vm:path` naming a path inside the VM.

use anyhow::Result;

use crate::hinted;
use crate::ssh;
use crate::vx::validate_name;

/// One argument to `vx cp`.
#[derive(Debug, PartialEq, Eq)]
pub enum Place {
    Local(String),
    /// A path in a VM; empty means its home directory, and relative paths start there too.
    Vm {
        name: String,
        path: String,
    },
}

impl Place {
    /// `dev:/tmp` is a path in VM `dev` when that VM exists. Anything else is a local path,
    /// including `./dev:x` (a slash before the colon) for a local file with a colon in its name.
    pub fn parse(arg: &str, exists: impl Fn(&str) -> bool) -> Result<Place> {
        let Some((name, path)) = arg.split_once(':') else { return Ok(Place::Local(arg.into())) };
        if name.contains('/') || validate_name(name).is_err() {
            return Ok(Place::Local(arg.into()));
        }
        if !exists(name) {
            return Err(hinted(
                format!("no VM named `{name}`"),
                format!("`vx ls` lists your VMs; for a local file, write ./{arg}"),
            ));
        }
        Ok(Place::Vm { name: name.into(), path: path.into() })
    }
}

/// What to copy: the VM it involves, and `scp`'s arguments for the sources and destination.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub vm: String,
    pub operands: Vec<String>,
}

/// Check the places make sense together: one VM involved, on one side or the other.
pub fn plan(places: Vec<Place>) -> Result<Plan> {
    let vms: Vec<&str> = places
        .iter()
        .filter_map(|p| match p {
            Place::Vm { name, .. } => Some(name.as_str()),
            Place::Local(_) => None,
        })
        .collect();
    let Some(vm) = vms.first().map(|v| v.to_string()) else {
        return Err(hinted("neither side is in a VM", "write the VM's side as <vm>:<path>, e.g. dev:/tmp"));
    };
    if let Some(other) = vms.iter().find(|n| **n != vm) {
        return Err(hinted(
            format!("can't copy between two VMs ({vm} and {other}) in one go"),
            "copy to this machine first, then on to the other VM",
        ));
    }
    let (dest, sources) = places.split_last().expect("clap requires two or more");
    let remote_sources = sources.iter().filter(|p| matches!(p, Place::Vm { .. })).count();
    if matches!(dest, Place::Vm { .. }) && remote_sources > 0 {
        return Err(hinted("both sides are in the VM", format!("use `vx ssh {vm} -- cp …` to copy within it")));
    }
    if matches!(dest, Place::Local(_)) && remote_sources < sources.len() {
        return Err(hinted("copy into the VM or out of it, not both at once", "split it into one `vx cp` each way"));
    }
    let operands = places
        .into_iter()
        .map(|p| match p {
            Place::Local(path) => path,
            Place::Vm { name, path } => format!("{}:{path}", ssh::alias(&name)),
        })
        .collect();
    Ok(Plan { vm, operands })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places(args: &[&str]) -> Result<Vec<Place>> {
        args.iter().map(|a| Place::parse(a, |name| ["dev", "web"].contains(&name))).collect()
    }

    fn operands(args: &[&str]) -> Result<Vec<String>> {
        plan(places(args)?).map(|p| p.operands)
    }

    #[test]
    fn vm_paths_need_an_existing_vm() {
        let vm = |name: &str, path: &str| Place::Vm { name: name.into(), path: path.into() };
        let local = |path: &str| Place::Local(path.into());
        assert_eq!(
            places(&["dev:/tmp", "dev:", "notes.txt"]).unwrap(),
            [vm("dev", "/tmp"), vm("dev", ""), local("notes.txt")]
        );
        // Slashes before the colon, or no name before it, make it local, as with scp.
        assert_eq!(places(&["./dev:x", "/a/b:c", ":x"]).unwrap(), [local("./dev:x"), local("/a/b:c"), local(":x")]);
        // Not a VM name at all: a local file with a colon in it.
        assert_eq!(places(&["Notes:v2.txt"]).unwrap(), [local("Notes:v2.txt")]);
        let e = places(&["prod:/etc"]).unwrap_err();
        assert_eq!(e.to_string(), "no VM named `prod`");
    }

    #[test]
    fn copies_in_and_out() {
        assert_eq!(operands(&["a.txt", "dir", "dev:/tmp/"]).unwrap(), ["a.txt", "dir", "dev.vx:/tmp/"]);
        assert_eq!(operands(&["dev:out.log", "dev:", "."]).unwrap(), ["dev.vx:out.log", "dev.vx:", "."]);
        assert_eq!(plan(places(&["dev:x", "."]).unwrap()).unwrap().vm, "dev");
    }

    #[test]
    fn refuses_what_scp_would_do_strangely() {
        let why = |args: &[&str]| operands(args).unwrap_err().to_string();
        assert_eq!(why(&["a", "b"]), "neither side is in a VM");
        assert_eq!(why(&["dev:a", "web:/tmp"]), "can't copy between two VMs (dev and web) in one go");
        assert_eq!(why(&["dev:a", "dev:/tmp"]), "both sides are in the VM");
        assert_eq!(why(&["dev:a", "b", "."]), "copy into the VM or out of it, not both at once");
    }
}
