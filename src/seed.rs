//! The cloud-init NoCloud seed: a small FAT disk labelled CIDATA that cloud-init reads on
//! first boot to create your user, install keys and set the hostname. It stays attached for
//! the VM's whole life: if it disappears, cloud-init disables itself.

use std::fs::{self, File};
use std::io::{Cursor, Read, Write};

use anyhow::{Context, Result};
use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};

use crate::ssh::HostKey;
use crate::vx::Vm;

const SIZE: usize = 2 << 20;

pub fn write(vm: &Vm, authorized_key: &str, host_key: &HostKey) -> Result<()> {
    let files = [
        ("meta-data", meta_data(&vm.name, &instance_id()?)),
        ("user-data", user_data(&vm.spec.ssh.user, authorized_key, host_key)),
        // Always present: cloud-init 24.3+ can wait for a missing one. Empty means DHCP.
        ("network-config", String::new()),
    ];
    let path = vm.path("seed.img");
    fs::write(&path, fat_image(&files)?).with_context(|| format!("writing {}", path.display()))
}

fn fat_image(files: &[(&str, String)]) -> Result<Vec<u8>> {
    let mut disk = Cursor::new(vec![0; SIZE]);
    let options = FormatVolumeOptions::new().volume_label(*b"CIDATA     ").fat_type(FatType::Fat12);
    fatfs::format_volume(&mut disk, options).context("formatting the seed image")?;
    let fs = FileSystem::new(&mut disk, FsOptions::new())?;
    for (name, content) in files {
        fs.root_dir().create_file(name)?.write_all(content.as_bytes())?;
    }
    fs.unmount()?;
    Ok(disk.into_inner())
}

/// Random and fixed for the VM's life: a new instance-id makes cloud-init run first boot again.
fn instance_id() -> Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn meta_data(name: &str, instance_id: &str) -> String {
    format!("instance-id: {instance_id}\nlocal-hostname: {name}\n")
}

fn user_data(user: &str, authorized_key: &str, host_key: &HostKey) -> String {
    let private: String = host_key.private.lines().map(|l| format!("    {l}\n")).collect();
    let public = host_key.public.trim();
    let authorized_key = authorized_key.trim();
    format!(
        r#"#cloud-config
# No "default" entry, so the image's stock user (debian, ubuntu) isn't created.
users:
  - name: {user}
    shell: /bin/bash
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    lock_passwd: true
    ssh_authorized_keys:
      - "{authorized_key}"
ssh_pwauth: false
manage_etc_hosts: true
# A host key vx generated, so known_hosts is pinned before the first connection.
ssh_keys:
  ed25519_private: |
{private}  ed25519_public: "{public}"
# While the host sleeps, the guest clock falls behind. Step it forward to the RTC,
# which keeps tracking host time. Never steps backwards.
write_files:
  - path: /usr/local/sbin/vx-clock
    permissions: "0755"
    content: |
      #!/bin/sh
      rtc=$(cat /sys/class/rtc/rtc0/since_epoch) now=$(date +%s)
      if [ "$((rtc - now))" -gt 2 ]; then date -s "@$rtc" >/dev/null; fi
  - path: /etc/systemd/system/vx-clock.service
    content: |
      [Service]
      Type=oneshot
      ExecStart=/usr/local/sbin/vx-clock
  - path: /etc/systemd/system/vx-clock.timer
    content: |
      [Timer]
      OnBootSec=10s
      OnUnitActiveSec=10s
      AccuracySec=1s
      [Install]
      WantedBy=timers.target
runcmd:
  - [systemctl, enable, --now, vx-clock.timer]
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_key() -> HostKey {
        HostKey {
            private: "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n-----END OPENSSH PRIVATE KEY-----\n".into(),
            public: "ssh-ed25519 AAAAhost dev.vx\n".into(),
        }
    }

    #[test]
    fn user_data_indents_the_private_key() {
        let text = user_data("alexa", "ssh-ed25519 AAAAclient vx\n", &host_key());
        assert!(text.starts_with("#cloud-config\n"));
        assert!(text.contains("  - name: alexa\n"));
        assert!(text.contains("      - \"ssh-ed25519 AAAAclient vx\"\n"));
        assert!(text.contains(
            "  ed25519_private: |\n    -----BEGIN OPENSSH PRIVATE KEY-----\n    b3BlbnNzaA==\n    -----END OPENSSH PRIVATE KEY-----\n  ed25519_public: \"ssh-ed25519 AAAAhost dev.vx\"\n"
        ));
    }

    #[test]
    fn fat_image_has_label_and_files() {
        let files = [("meta-data", meta_data("dev", "abc")), ("network-config", String::new())];
        let mut disk = Cursor::new(fat_image(&files).unwrap());
        let fs = FileSystem::new(&mut disk, FsOptions::new()).unwrap();
        assert_eq!(fs.volume_label(), "CIDATA");
        assert_eq!(fs.fat_type(), FatType::Fat12);
        let mut text = String::new();
        fs.root_dir().open_file("meta-data").unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "instance-id: abc\nlocal-hostname: dev\n");
        assert!(fs.root_dir().open_file("network-config").is_ok());
    }
}
