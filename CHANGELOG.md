# Changelog

The Release workflow uses the section matching each version as its GitHub Release notes.

## Unreleased

## 0.2.0

- **Snapshots.** `vx snap <vm> [name] [-m note]` saves a VM as it is; `vx snap ls`, `vx snap restore` and `vx snap rm` list, go back to and delete them. A running VM's memory is saved too, so going back resumes it exactly where it was. Snapshots are independent save slots: going back to one or deleting one never touches the others.
- **Snapshots in the dashboard.** `S` opens the selected VM's snapshots, `ctrl-s` saves one right away, and the snapshot the VM came from is highlighted.
- **Live details pane.** On terminals at least 120 columns wide, the dashboard shows the selected VM's CPU (overall and per core), memory, swap, disk and network from inside the guest, plus its newest snapshots. `i` hides or shows it.
- Upgrading on macOS: VMs still running from 0.1.0 need one restart (`vx stop`, `vx start`) before a snapshot can save their memory; until then they get disk-only snapshots.

## 0.1.0

First release.

- `vx new`, `ssh`, `start`, `stop`, `pause`, `resume`, `console`, `logs` and `rm`, with a VM picker when the name is left out.
- A dashboard (`vx`) with VMs and images views, and a form for new VMs.
- 16 built-in cloud images, downloaded once and checked against their published checksums.
- Bring your own image with `vx images add`.
