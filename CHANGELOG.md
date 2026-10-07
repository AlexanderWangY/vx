# Changelog

The Release workflow uses the section matching each version as its GitHub Release notes.

## Unreleased

- **Rename a VM.** `vx mv dev web` (or `vx rename`) renames it, along with its hostname and its `web.vx` SSH alias. A running VM restarts to take the new name, after asking; `-y` doesn't ask. In the dashboard, `r` renames the selected VM.

- **Change a VM's CPUs, memory and disk.** `vx set dev --cpus 8 --mem 16G --disk 40G`, or `--disk +20G` for that much more; `vx set dev` shows them. A disk grows straight away, even while the VM runs, and so does its filesystem. New CPUs and memory take effect on restart, which `vx set` offers, or `--restart` does. In the dashboard, `e` changes them.

- **Clones.** `vx clone dev` makes `dev-2`, a new VM with a copy of `dev`'s disk and a name, hostname, SSH port and host key of its own; `vx clone dev web` names it, and `vx clone dev@deps` copies `dev` as it was at a snapshot. A stopped VM is copied instantly; a running one keeps running. In the dashboard, `C` clones the selected VM, or the selected snapshot.
- `vx snap` has a running VM write what it's holding in memory to its disk first, so a snapshot's disk is complete on its own, for cloning.

- **Shared folders.** `vx mount dev ~/code` makes `~/code` here show up at `~/code` in `dev`, live both ways, or `vx mount dev .:/srv/app` puts it somewhere else. `--ro` shares it read-only, `vx mount dev` lists them, `vx mount rm dev ~/code` stops sharing one, and `vx new --mount` shares one from the start. A running VM gets it straight away, and it's saved in `vx.toml` for next time. It works on every built-in distro: sshfs over vx's SSH connection, installed in the VM the first time.
- `sshfs` is one of the package names `vx install` translates for each distro.

## 0.3.0

- **New VMs come set up the way you like.** List packages in `~/.vx/config.toml` (`[new] install = ["git", "build-tools"]`) and every `vx new` installs them once the VM is up, then runs your own setup script if you give one (`setup = "~/.vx/setup.sh"`, run as you). `build-tools`, `python`, `node`, `go`, `rust` and `fd` work on every distro; other names are the distro's own. `vx new --install`, `--setup` and `--bare` change it for one VM, the dashboard's new-VM form shows it, and `vx install <vm> <packages>` installs into an existing VM.
- **Port forwarding.** `vx port dev 8080:80` makes `localhost:8080` reach port 80 in `dev`, or `vx port dev 3000` uses the same port on both sides. A running VM picks it up straight away, and it's saved in `vx.toml` for next time. `vx port dev` lists them, `vx port rm dev 8080` removes one, and `f` in the dashboard does all three.
- **Copying files.** `vx cp` copies files and directories into or out of a VM: `vx cp notes.txt src/ dev:/tmp/`, `vx cp dev:out.log .`. It starts the VM first if it's stopped.
- The highlighted snapshot is now called the current snapshot: the one the VM's state is based on.
- `vx ssh` to a paused VM says to resume it, instead of hanging.

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
