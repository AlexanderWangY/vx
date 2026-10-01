use std::path::PathBuf;

/// A VM is a directory: $VX_HOME/vms/<name>/
pub struct Vm {
    pub name: String,
    pub dir: PathBuf,
    // TODO: `spec: Spec`, loaded from vx.toml
}
