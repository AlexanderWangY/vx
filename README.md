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

`⏎` ssh · `s` start · `x` stop · `p` pause · `n` new · `d` delete · `S` snapshots · `tab` images · `?` all keys

On a terminal at least 120 columns wide, the selected VM's details sit on the right: live CPU (overall and per core), memory, swap, disk and network from inside the guest, read over SSH with nothing to install. `i` hides or shows them.

![Images](assets/images_ss.jpg)

## Commands

```
vx new web --image fedora-44 --cpus 2 --mem 8G --disk 40G
vx new                          # no name: fill in a form
vx install <vm> <package>...    # see "Set up new VMs your way" below
vx ls
vx set <vm> [--cpus 8] [--mem 16G] [--disk +20G]   # change them, or see them
vx start | stop | pause | resume <vm>
vx ssh <vm> [-- cmd]            # starts it first if needed
vx cp <path>... <vm>:<path>     # copy in; or <vm>:<path>... <path> to copy out
vx port <vm> [8080:80 | 3000]   # forward ports to it, or list them; vx port rm <vm> 8080
vx mount <vm> [~/code]          # share a folder with it, or list them; see "Shared folders" below
vx console <vm>                 # serial console, Ctrl-] to detach
vx logs -f <vm>
vx snap <vm> [name]             # see Snapshots below
vx clone <vm>[@snapshot] [name] # a new VM that's a copy; see Clones below
vx mv <vm> <new-name>           # rename it; its hostname follows
vx rm <vm>
```

Leave out the VM name and you get a picker.

## Set up new VMs your way

Have every new VM come with your tools, on any distro:

```toml
# ~/.vx/config.toml
[new]
install = ["git", "build-tools", "python", "tmux"]
setup = "~/.vx/setup.sh"     # optional: a script of yours, run in the VM as you
```

`vx new` installs the packages once the VM is up, then runs the script, showing progress as it goes. Package names are the distro's own, except a few that differ between distros, which work everywhere: `build-tools`, `python`, `node`, `go`, `rust`, `fd` and `sshfs`. On Rocky, Alma and CentOS, EPEL is turned on when a package needs it.

- `vx new dev --install htop,jq` adds to the defaults for one VM; `--bare` skips them; `--setup ./other.sh` runs a different script.
- `vx install dev ripgrep` installs into a VM you already have.
- In the dashboard, the new-VM form shows the defaults, ready to change for that VM.
- Everything they print goes to `~/.vx/vms/<vm>/setup.log`.

## Shared folders

Edit on your machine, build in the VM:

```
vx mount dev ~/code             # ~/code here is ~/code in dev, live both ways
vx mount dev .:/srv/app         # this folder, at /srv/app in dev
vx mount dev ~/notes --ro       # dev can read it but not change it
vx mount dev                    # list them
vx mount rm dev ~/code          # stop sharing it
vx new dev --mount ~/code       # or share it from the start
```

A folder in your home goes to the same place in the VM's home; one outside it keeps its path. A running VM gets it straight away, and it comes back every time the VM starts. Files you create in the VM are yours on this machine, and yours in the VM.

It works on every built-in distro and needs nothing set up on either side: the VM mounts the folder with sshfs over vx's own SSH connection, served by this machine's `sftp-server`, so nothing new listens on your network. The first mount installs sshfs in the VM. It's quick for editing and building, though slower than the VM's own disk for heavy I/O, like a large `node_modules`.

One thing to know: `sftp-server` runs as you, and root in the VM could use the connection to reach any file you can, not only the shared folder. Share folders with VMs you'd trust with your own shell.

## Snapshots

```
vx snap dev before-upgrade -m "known good"   # save it as it is now
vx snap ls dev                               # list them
vx snap restore dev before-upgrade           # go back
vx snap rm dev before-upgrade
```

Think of snapshots as save slots: each is the whole VM at one moment, and going back to one or deleting one never touches the others. A running VM's memory is saved too, so going back puts it exactly where it was, processes and all.

```
fresh       2d ago      disk only  clean install
deps        2d ago  545 MB memory  toolchains
try-nix     1d ago  547 MB memory
k8s         5h ago  547 MB memory  from deps · kind cluster up
```

The current snapshot, the one the VM's state is based on, is highlighted. `from deps` appears only after going back: when a snapshot wasn't saved right after the one above it.

In the dashboard, `S` opens the selected VM's snapshots and `ctrl-s` saves one right away. Snapshots live inside the VM's disk, so `vx rm` takes them with it.

## Changing CPUs, memory and disk

```
vx set dev                      # dev: 4 CPUs · 4G memory · 20G disk
vx set dev --cpus 8 --mem 16G
vx set dev --disk 40G           # or --disk +20G for 20G more
```

A disk grows straight away, even while the VM runs, and so does its filesystem. Disks only grow. New CPUs and memory take effect when the VM restarts: `vx set` asks whether to restart it now, or `--restart` does without asking.

In the dashboard, `e` changes the selected VM's.

## Clones

Set a VM up once, then make as many as you like:

```
vx clone dev                    # dev-2, a copy of dev as it is now
vx clone dev web                # named web
vx clone dev@deps try-nix       # dev as it was at snapshot deps
```

A clone starts with everything on the original's disk, as a machine of its own: its own name and hostname, SSH port and host key. The original's snapshots and forwarded ports stay with it; shared folders come along.

The original can keep running. A stopped one is copied instantly, sharing disk space until either changes. A running one keeps running, and the clone gets its disk as it is at that moment. Cloning a snapshot copies its disk, so the clone boots fresh even if the snapshot saved memory too.

In the dashboard, `C` clones the selected VM, or the selected snapshot on the snapshots screen.

## Images

16 popular distros are built in. `vx images` lists them. Each is downloaded once, checked against its published checksum, and cached.

Bring your own qcow2 or raw image:

```
vx images add mybox ~/Downloads/mybox.qcow2
vx new dev --image mybox
```

## Tricks

- `vx mv dev web` renames a VM, along with its hostname and its `ssh web.vx` alias. A running VM restarts to take the new name; `vx mv` asks first. In the dashboard, `r` renames the selected VM. Going back to a snapshot saved before the rename brings back the old hostname until the VM restarts.
- `vx ssh dev -- uname -a` runs one command.
- `vx cp ./src dev:` copies a whole directory into your home in the VM; `vx cp dev:build/app.log .` brings a file back. A path on the VM's side is relative to your home there.
- Put `Include ~/.vx/vms/*/ssh_config` at the top of `~/.ssh/config`, then `ssh dev.vx`, `scp` and `rsync` just work.
- `vx port dev 8080:80` makes `localhost:8080` reach port 80 in `dev`, straight away if it's running and every time it starts. Forwards listen on 127.0.0.1 only, so nothing else on your network can reach them. In the dashboard, `f` shows and changes them.
- `vx images pull ubuntu-24.04` downloads ahead of time.
- `VX_HOME=/somewhere/else` keeps everything somewhere else.
