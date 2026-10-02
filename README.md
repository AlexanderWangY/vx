<p align="center">
  <img src="assets/vx_logo.png" alt="vx logo" width="128">
</p>

# vx

**V**M Multiple**X**er

After writing one too many bash scripts to manage my QEMU VMs, I vibecoded `vx` to be the zero-config VM management CLI and TUI of your dreams! It has *never* been easier to provision, manage, and ssh into Linux VMs from the terminal.

```
vx new dev    # create and boot a VM
vx ssh dev    # connect to it
vx            # open the dashboard
```

vx uses QEMU under the hood, needs no sudo or config file, and keeps each VM in its own directory under `~/.vx`.

Works on macOS and Linux.

## Install

Needs QEMU (`brew install qemu`, or your package manager) and Rust.

```
cargo install --git https://github.com/AlexanderWangY/vx
```

## Dashboard

Run `vx` with no arguments.

![VMs](assets/dashboard_ss.jpg)

`⏎` ssh · `s` start · `x` stop · `p` pause · `n` new · `d` delete · `t` snapshots · `tab` images · `?` all keys

On a terminal at least 120 columns wide, the selected VM's details sit on the right: live CPU (overall and per core), memory, swap, disk and network from inside the guest, read over SSH with nothing to install. `i` hides or shows them.

![Images](assets/images_ss.jpg)

## Commands

```
vx new web --image fedora-44 --cpus 2 --mem 8G --disk 40G
vx new                          # no name: fill in a form
vx ls
vx start | stop | pause | resume <vm>
vx ssh <vm> [-- cmd]            # starts it first if needed
vx console <vm>                 # serial console, Ctrl-] to detach
vx logs -f <vm>
vx snap <vm> [name]             # see Snapshots below
vx rm <vm>
```

Leave out the VM name and you get a picker.

## Snapshots

```
vx snap dev before-upgrade -m "known good"   # save it as it is now
vx snap ls dev                               # the history, as a tree
vx snap restore dev before-upgrade           # go back
vx snap rm dev before-upgrade
```

A running VM's memory is saved too, so restoring puts it back exactly where it was, processes and all. Restore an older snapshot and save again, and you've started a branch. `vx snap ls` keeps the line to where the VM is now straight down the left, with everything else branching off it:

```
○ fresh       2d ago                    first boot
● deps        2d ago  +545 MB memory    toolchains
├─● try-nix   1d ago  +547 MB memory
● k8s         5h ago  +547 MB memory    kind cluster up
◉ now         running
```

In the dashboard, `t` opens the selected VM's tree: `c` saves, `⏎` restores, `space` unfolds a branch. Snapshots live inside the VM's disk, so `vx rm` takes them with it.

## Images

16 popular distros are built in. `vx images` lists them. Each is downloaded once, checked against its published checksum, and cached.

Bring your own qcow2 or raw image:

```
vx images add mybox ~/Downloads/mybox.qcow2
vx new dev --image mybox
```

## Tricks

- `vx ssh dev -- uname -a` runs one command.
- Put `Include ~/.vx/vms/*/ssh_config` at the top of `~/.ssh/config`, then `ssh dev.vx`, `scp` and `rsync` just work.
- Forward ports: add `forward = ["8080:80"]` above `[ssh]` in `~/.vx/vms/dev/vx.toml`. Applies on next start.
- `vx images pull ubuntu-24.04` downloads ahead of time.
- `VX_HOME=/somewhere/else` keeps everything somewhere else.
