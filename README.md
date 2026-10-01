<p align="center">
  <img src="assets/vx_logo.png" alt="vx logo" width="128">
</p>

# vx

A small, zero-config tool for running Linux VMs from the terminal, written in Rust.

```
vx new dev    # create and boot a VM
vx ssh dev    # connect to it
vx            # open the dashboard
```

vx uses QEMU under the hood, needs no sudo or config file, and keeps each VM in its own directory under `~/.vx`.

Works on macOS and Linux.
