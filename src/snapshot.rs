//! Snapshots as save slots: each is the whole VM at one moment, independent of the others, so
//! going back to one or deleting one never changes another. The backend keeps them (QEMU:
//! inside disk.qcow2); vx adds what it doesn't record in the VM's snapshots.toml: a note, which
//! snapshot each was saved from, and the current snapshot: the one the VM's state is based on,
//! because it was the last one saved or gone back to.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::backend::{Snap, Snapshots};
use crate::hinted;
use crate::vx::Vm;

const FILE: &str = "snapshots.toml";
const HEADER: &str = "# Written by vx: how this VM's snapshots relate. The snapshots are in its disk.\n";

/// One snapshot: what the backend knows, plus what vx recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub snap: Snap,
    pub parent: Option<String>,
    pub note: String,
}

/// A VM's snapshots, oldest first, and the current one.
#[derive(Debug, Default)]
pub struct History {
    pub entries: Vec<Entry>,
    /// The snapshot the VM was last saved or restored at; `None` before the first.
    pub current: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current: Option<String>,
    #[serde(default, rename = "snapshot")]
    snapshots: Vec<Record>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    note: String,
}

impl History {
    /// The backend's snapshots, related by what snapshots.toml recorded.
    pub fn load(vm: &Vm, backend: &dyn Snapshots) -> Result<History> {
        let path = vm.path(FILE);
        let file: File = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?,
            Err(_) => File::default(),
        };
        Ok(History::from_parts(backend.list(vm)?, &file))
    }

    /// The backend's list is the truth: records of snapshots that are gone are dropped, and
    /// anything that pointed at one points at its nearest surviving ancestor instead.
    fn from_parts(snaps: Vec<Snap>, file: &File) -> History {
        let records: HashMap<&str, &Record> = file.snapshots.iter().map(|r| (r.name.as_str(), r)).collect();
        let exists: HashSet<&str> = snaps.iter().map(|s| s.name.as_str()).collect();
        let survivor = |name: Option<&str>| nearest(name, &records, &exists);
        let entries = snaps
            .iter()
            .map(|snap| {
                let record = records.get(snap.name.as_str());
                let parent = survivor(record.and_then(|r| r.parent.as_deref())).filter(|p| *p != snap.name);
                Entry { snap: snap.clone(), parent, note: record.map(|r| r.note.clone()).unwrap_or_default() }
            })
            .collect();
        History { entries, current: survivor(file.current.as_deref()) }
    }

    pub fn save(&self, vm: &Vm) -> Result<()> {
        let file = File {
            current: self.current.clone(),
            snapshots: self
                .entries
                .iter()
                .map(|e| Record { name: e.snap.name.clone(), parent: e.parent.clone(), note: e.note.clone() })
                .collect(),
        };
        let text = format!("{HEADER}{}", toml::to_string(&file)?);
        let (path, tmp) = (vm.path(FILE), vm.path(".snapshots.toml.tmp"));
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.snap.name == name)
    }

    /// The snapshot called `name`, or an error saying how to see the ones there are.
    pub fn find(&self, vm: &Vm, name: &str) -> Result<&Entry> {
        self.get(name).ok_or_else(|| {
            hinted(format!("{} has no snapshot called `{name}`", vm.name), format!("vx snap ls {}", vm.name))
        })
    }

    /// `snap-1`, `snap-2`, …: the first that isn't taken.
    pub fn next_name(&self) -> String {
        (1..).map(|n| format!("snap-{n}")).find(|n| self.get(n).is_none()).unwrap()
    }

    /// Record a snapshot just saved: it follows on from where the VM was, and is where it is now.
    pub fn add(&mut self, snap: Snap, note: String) {
        let parent = self.current.replace(snap.name.clone());
        self.entries.push(Entry { snap, parent, note });
    }

    /// Forget a deleted snapshot. What was saved from it now counts as saved from where it
    /// was saved from, and the same goes for the VM.
    pub fn remove(&mut self, name: &str) {
        let Some(i) = self.entries.iter().position(|e| e.snap.name == name) else { return };
        let gone = self.entries.remove(i);
        for e in &mut self.entries {
            if e.parent.as_deref() == Some(name) {
                e.parent = gone.parent.clone();
            }
        }
        if self.current.as_deref() == Some(name) {
            self.current = gone.parent;
        }
    }

    /// Where snapshot `i` was saved from, when that isn't the snapshot listed just before it:
    /// after going back to an older one. A straight run of snapshots never needs saying.
    pub fn from(&self, i: usize) -> Option<&str> {
        let parent = self.entries[i].parent.as_deref()?;
        let previous = i.checked_sub(1).map(|p| self.entries[p].snap.name.as_str());
        (previous != Some(parent)).then_some(parent)
    }
}

/// `name` if it exists, otherwise its nearest recorded ancestor that does.
fn nearest<'a>(
    mut name: Option<&'a str>,
    records: &HashMap<&'a str, &'a Record>,
    exists: &HashSet<&str>,
) -> Option<String> {
    let mut seen = HashSet::new();
    while let Some(n) = name {
        if exists.contains(n) {
            return Some(n.to_string());
        }
        if !seen.insert(n) {
            return None; // a loop in a hand-edited file
        }
        name = records.get(n).and_then(|r| r.parent.as_deref());
    }
    None
}

/// A snapshot's marker: filled when its memory was saved too.
pub fn marker(entry: &Entry) -> char {
    if entry.snap.memory > 0 { '●' } else { '○' }
}

pub fn validate_name(name: &str) -> Result<()> {
    // Starting with a letter also keeps it from being read as one of QEMU's numeric snapshot IDs.
    let ok = name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.');
    if !ok {
        return Err(hinted(
            format!("`{name}` isn't a valid snapshot name"),
            "use up to 32 lowercase letters, digits, `-` and `.`, starting with a letter",
        ));
    }
    Ok(())
}

/// `5m ago`
pub fn ago(created: u64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let s = now.saturating_sub(created);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str, created: u64) -> Snap {
        Snap { name: name.into(), created, memory: 0 }
    }

    /// fresh → deps → { try-nix → nix-2, k8s }, at k8s.
    fn history() -> History {
        let mut h = History::default();
        h.add(snap("fresh", 1), "first boot".into());
        h.add(snap("deps", 2), String::new());
        h.add(snap("try-nix", 3), String::new());
        h.add(snap("nix-2", 4), String::new());
        h.current = Some("deps".into()); // restored deps
        h.add(snap("k8s", 5), String::new());
        h
    }

    /// Each snapshot with its `from` note, and `*` after the current one.
    fn list(h: &History) -> Vec<String> {
        (0..h.entries.len())
            .map(|i| {
                let name = &h.entries[i].snap.name;
                let here = if h.current.as_deref() == Some(name.as_str()) { "*" } else { "" };
                match h.from(i) {
                    Some(from) => format!("{name}{here} (from {from})"),
                    None => format!("{name}{here}"),
                }
            })
            .collect()
    }

    #[test]
    fn from_only_shows_after_going_back() {
        let h = history();
        assert_eq!(list(&h), ["fresh", "deps", "try-nix", "nix-2", "k8s* (from deps)"]);
        let mut straight = History::default();
        for (i, name) in ["a", "b", "c"].into_iter().enumerate() {
            straight.add(snap(name, i as u64), String::new());
        }
        assert_eq!(list(&straight), ["a", "b", "c*"]);
    }

    #[test]
    fn removing_reparents() {
        let mut h = history();
        h.remove("deps");
        assert_eq!(h.get("try-nix").unwrap().parent.as_deref(), Some("fresh"));
        assert_eq!(h.get("k8s").unwrap().parent.as_deref(), Some("fresh"));
        assert_eq!(list(&h), ["fresh", "try-nix", "nix-2", "k8s* (from fresh)"]);
        h.remove("k8s");
        assert_eq!(h.current.as_deref(), Some("fresh"));
        assert_eq!(list(&h), ["fresh*", "try-nix", "nix-2"]);
    }

    #[test]
    fn the_backend_list_wins() {
        let file: File = toml::from_str(
            r#"
            current = "gone"
            [[snapshot]]
            name = "a"
            note = "kept"
            [[snapshot]]
            name = "gone"
            parent = "a"
            [[snapshot]]
            name = "b"
            parent = "gone"
            "#,
        )
        .unwrap();
        // `gone` was deleted outside vx, and `c` was made outside it.
        let h = History::from_parts(vec![snap("a", 1), snap("b", 2), snap("c", 3)], &file);
        assert_eq!(h.current.as_deref(), Some("a"));
        assert_eq!(h.get("a").unwrap().note, "kept");
        assert_eq!(h.get("b").unwrap().parent.as_deref(), Some("a"));
        assert_eq!(h.get("c").unwrap().parent, None);
        assert_eq!(list(&h), ["a*", "b", "c"]);
    }

    #[test]
    fn nothing_yet() {
        let h = History::default();
        assert!(list(&h).is_empty());
        assert_eq!(h.next_name(), "snap-1");
        assert_eq!(history().next_name(), "snap-1");
    }

    #[test]
    fn names() {
        assert!(validate_name("deps-2.1").is_ok());
        for bad in ["", "1", "Deps", "-x", "a b", &"x".repeat(33)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }
}
