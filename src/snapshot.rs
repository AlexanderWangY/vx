//! Snapshot history. The backend keeps the snapshots themselves (QEMU: inside disk.qcow2) as a
//! flat list; vx adds how they relate in the VM's snapshots.toml: each one's parent, a note,
//! and which one the VM was last saved or restored at. Restoring an older snapshot and saving
//! again starts a branch, so the history is a tree.

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

/// A VM's snapshots, oldest first, and where the VM is in their tree.
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

    /// Forget a deleted snapshot. Its children move up to its parent, and so does the VM if
    /// it was there.
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

    /// How many snapshots descend from `name`.
    pub fn descendants(&self, name: &str) -> usize {
        self.children(Some(name)).iter().map(|&i| 1 + self.descendants(&self.entries[i].snap.name)).sum()
    }

    /// Indexes of the snapshots whose parent is `parent`, oldest first.
    fn children(&self, parent: Option<&str>) -> Vec<usize> {
        (0..self.entries.len()).filter(|&i| self.entries[i].parent.as_deref() == parent).collect()
    }

    /// The snapshots from the root down to where the VM is.
    fn path(&self) -> HashSet<&str> {
        let mut path = HashSet::new();
        let mut at = self.current.as_deref();
        while let Some(name) = at
            && path.insert(name)
        {
            at = self.get(name).and_then(|e| e.parent.as_deref());
        }
        path
    }

    /// The tree as rows, top to bottom. The line from the first snapshot down to where the VM
    /// is now runs straight down the left; everything else branches off it, so a long history
    /// without branches never drifts right. Branches show their first snapshot, and the rest
    /// only if `open` says so.
    pub fn rows(&self, open: &dyn Fn(&str) -> bool) -> Vec<Row> {
        let path = self.path();
        let mut rows = Vec::new();
        self.walk(None, "", &path, open, &mut rows);
        rows
    }

    fn walk(
        &self,
        parent: Option<&str>,
        prefix: &str,
        path: &HashSet<&str>,
        open: &dyn Fn(&str) -> bool,
        rows: &mut Vec<Row>,
    ) {
        let kids = self.children(parent);
        let on_path = parent.is_none_or(|p| path.contains(p));
        // What carries on straight down: towards where the VM is, or in a branch, its newest snapshot.
        let main = if on_path {
            kids.iter().copied().find(|&i| path.contains(self.entries[i].snap.name.as_str()))
        } else {
            kids.last().copied()
        };
        for &i in kids.iter().filter(|&&i| Some(i) != main) {
            let name = &self.entries[i].snap.name;
            let hidden = self.descendants(name);
            let is_open = hidden > 0 && open(name);
            let fold = (hidden > 0).then_some(Fold { open: is_open, hidden });
            rows.push(Row { graph: format!("{prefix}├─"), at: At::Snapshot(i), fold });
            if is_open {
                self.walk(Some(name), &format!("{prefix}│ "), path, open, rows);
            }
        }
        match main {
            Some(i) => {
                rows.push(Row { graph: prefix.to_string(), at: At::Snapshot(i), fold: None });
                self.walk(Some(&self.entries[i].snap.name), prefix, path, open, rows);
            }
            // The end of the line to where the VM is.
            None if on_path && parent == self.current.as_deref() => {
                rows.push(Row { graph: prefix.to_string(), at: At::Now, fold: None });
            }
            None => {}
        }
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

/// One line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The lines drawn before it, e.g. `│ ├─`.
    pub graph: String,
    pub at: At,
    /// For the first snapshot of a branch that has more.
    pub fold: Option<Fold>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    /// An index into `History::entries`.
    Snapshot(usize),
    /// Where the VM is now.
    Now,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    pub open: bool,
    /// Snapshots in the branch after its first.
    pub hidden: usize,
}

/// A snapshot's marker: filled when its memory was saved too.
pub fn marker(entry: &Entry) -> char {
    if entry.snap.memory > 0 { '●' } else { '○' }
}

pub const NOW: char = '◉';

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

    /// The rows as text, the way `vx snap ls` draws them.
    fn draw(h: &History, open: &dyn Fn(&str) -> bool) -> Vec<String> {
        h.rows(open)
            .iter()
            .map(|row| {
                let fold = match row.fold {
                    Some(Fold { open: true, .. }) => "▾",
                    Some(Fold { open: false, .. }) => "▸",
                    None => "",
                };
                match row.at {
                    At::Snapshot(i) => format!("{}{fold}○ {}", row.graph, h.entries[i].snap.name),
                    At::Now => format!("{}◉ now", row.graph),
                }
            })
            .collect()
    }

    #[test]
    fn branches_hang_off_the_line_to_now() {
        let h = history();
        assert_eq!(draw(&h, &|_| false), ["○ fresh", "○ deps", "├─▸○ try-nix", "○ k8s", "◉ now"]);
        assert_eq!(draw(&h, &|_| true), ["○ fresh", "○ deps", "├─▾○ try-nix", "│ ○ nix-2", "○ k8s", "◉ now"]);
        assert_eq!(h.descendants("deps"), 3);
    }

    #[test]
    fn restoring_moves_the_line() {
        let mut h = history();
        h.current = Some("nix-2".into());
        assert_eq!(draw(&h, &|_| false), ["○ fresh", "○ deps", "├─○ k8s", "○ try-nix", "○ nix-2", "◉ now"]);
    }

    #[test]
    fn nested_branches() {
        let mut h = history();
        h.current = Some("try-nix".into());
        h.add(snap("nix-3", 6), String::new()); // a sibling of nix-2
        h.current = Some("k8s".into());
        assert_eq!(
            draw(&h, &|_| true),
            ["○ fresh", "○ deps", "├─▾○ try-nix", "│ ├─○ nix-2", "│ ○ nix-3", "○ k8s", "◉ now"]
        );
    }

    #[test]
    fn removing_reparents() {
        let mut h = history();
        h.remove("deps");
        assert_eq!(h.get("try-nix").unwrap().parent.as_deref(), Some("fresh"));
        assert_eq!(h.get("k8s").unwrap().parent.as_deref(), Some("fresh"));
        h.remove("k8s");
        assert_eq!(h.current.as_deref(), Some("fresh"));
        assert_eq!(draw(&h, &|_| true), ["○ fresh", "├─▾○ try-nix", "│ ○ nix-2", "◉ now"]);
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
        assert_eq!(draw(&h, &|_| true), ["├─○ c", "○ a", "├─○ b", "◉ now"]);
    }

    #[test]
    fn nothing_yet() {
        let h = History::default();
        assert_eq!(draw(&h, &|_| true), ["◉ now"]);
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
