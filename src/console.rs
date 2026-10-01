//! The serial console and boot log. Every backend exposes the guest's serial port the same way:
//! a socket to attach to, and everything it printed in `serial.log`.

use std::fs::File;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;
use std::{process, thread};

use anyhow::{Context, Result, ensure};

use crate::hinted;
use crate::style::ERR;
use crate::vx::Vm;

/// Ctrl-]
const DETACH: u8 = 0x1d;

/// Pass the terminal through to the guest's serial port until Ctrl-] or the VM goes away.
pub fn attach(name: &str, serial: UnixStream) -> Result<()> {
    ensure!(io::stdin().is_terminal(), "the console needs a terminal");
    eprintln!("{}", ERR.dim(format!("connected to {name}; press Enter for a prompt and Ctrl-] to detach")));
    let raw = RawMode::enable()?;

    let mut from_guest = serial.try_clone()?;
    let saved = raw.saved;
    let who = name.to_string();
    thread::spawn(move || {
        let mut out = io::stdout();
        let mut buf = [0; 4096];
        while let Ok(n @ 1..) = from_guest.read(&mut buf) {
            if out.write_all(&buf[..n]).and_then(|()| out.flush()).is_err() {
                break;
            }
        }
        restore(&saved);
        eprintln!("\n{}", ERR.dim(format!("{who} disconnected")));
        process::exit(0);
    });

    let mut to_guest = serial;
    let mut stdin = io::stdin().lock();
    let mut buf = [0; 1024];
    let result = loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(e) => break Err(e),
        };
        let chunk = &buf[..n];
        if let Some(i) = chunk.iter().position(|&b| b == DETACH) {
            break to_guest.write_all(&chunk[..i]);
        }
        if let Err(e) = to_guest.write_all(chunk) {
            break Err(e);
        }
    };
    drop(raw);
    eprintln!("\n{}", ERR.dim(format!("detached from {name}")));
    Ok(result?)
}

/// The terminal in raw mode: keys go straight to the guest, including Ctrl-C.
/// The original settings come back on drop, even on panic.
struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    fn enable() -> io::Result<RawMode> {
        // SAFETY: termios is plain data, and tcgetattr fills it in before we read it.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let saved = t;
        // SAFETY: `t` is a valid termios from tcgetattr.
        unsafe { libc::cfmakeraw(&mut t) };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(RawMode { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore(&self.saved);
    }
}

fn restore(saved: &libc::termios) {
    // SAFETY: `saved` came from tcgetattr.
    unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved) };
}

/// Print everything the guest wrote to its serial port since it last booted.
/// With `follow`, keep printing new output until interrupted.
pub fn logs(vm: &Vm, follow: bool) -> Result<()> {
    let path = vm.path("serial.log");
    let mut out = io::stdout().lock();
    let mut pos = 0;
    loop {
        match File::open(&path) {
            Ok(mut file) => {
                if file.metadata()?.len() < pos {
                    pos = 0; // a new boot started the log over
                }
                file.seek(SeekFrom::Start(pos))?;
                match io::copy(&mut file, &mut out).and_then(|n| out.flush().map(|()| n)) {
                    Ok(n) => pos += n,
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()), // e.g. `| head`
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && follow => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(hinted(
                    format!("{} hasn't been started yet, so it has no log", vm.name),
                    format!("vx start {}", vm.name),
                ));
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
        if !follow {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
}
