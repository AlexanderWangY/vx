//! A synchronous QMP client.
//!
//! QEMU serves one QMP client at a time, so every call is its own short session:
//! connect, read the greeting, negotiate capabilities, run one command, disconnect.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(2);

/// Run `cmd` and return its `return` value. Connection failures keep their `io::Error`,
/// so callers can tell "nothing listening" from "busy".
pub fn call(sock: &Path, cmd: &str) -> Result<Value> {
    let stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut q = Session { r: BufReader::new(stream.try_clone()?), w: stream };
    let greeting = q.read()?;
    ensure!(greeting.get("QMP").is_some(), "{} is not a QMP socket", sock.display());
    q.exec("qmp_capabilities")?;
    q.exec(cmd)
}

struct Session {
    r: BufReader<UnixStream>,
    w: UnixStream,
}

impl Session {
    fn exec(&mut self, cmd: &str) -> Result<Value> {
        writeln!(self.w, "{}", json!({ "execute": cmd }))?;
        loop {
            let mut msg = self.read()?;
            if msg.get("event").is_some() {
                continue; // events interleave with replies
            }
            if let Some(e) = msg.get("error") {
                bail!("QMP {cmd} failed: {}", e["desc"].as_str().unwrap_or("unknown error"));
            }
            return Ok(msg["return"].take());
        }
    }

    fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        ensure!(self.r.read_line(&mut line)? > 0, "QMP connection closed");
        Ok(serde_json::from_str(&line)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::{env, fs, io, thread};

    /// A fake QEMU: accepts one connection and plays `script` against it.
    fn serve(script: impl FnOnce(&mut BufReader<UnixStream>, &mut UnixStream) + Send + 'static) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let sock = env::temp_dir().join(format!("vx-qmp-{}-{n}.sock", std::process::id()));
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        thread::spawn(move || {
            let (mut w, _) = listener.accept().unwrap();
            let mut r = BufReader::new(w.try_clone().unwrap());
            script(&mut r, &mut w);
        });
        sock
    }

    fn expect(r: &mut BufReader<UnixStream>, cmd: &str) {
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let msg: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(msg["execute"], cmd);
    }

    const GREETING: &str = r#"{"QMP": {"version": {}, "capabilities": []}}"#;

    #[test]
    fn runs_a_command_and_skips_events() {
        let sock = serve(|r, w| {
            writeln!(w, "{GREETING}").unwrap();
            expect(r, "qmp_capabilities");
            writeln!(w, r#"{{"return": {{}}}}"#).unwrap();
            expect(r, "query-status");
            writeln!(w, r#"{{"event": "RESUME", "data": {{}}}}"#).unwrap();
            writeln!(w, r#"{{"return": {{"status": "running", "running": true}}}}"#).unwrap();
        });
        let reply = call(&sock, "query-status").unwrap();
        assert_eq!(reply["status"], "running");
        fs::remove_file(sock).unwrap();
    }

    #[test]
    fn reports_qmp_errors() {
        let sock = serve(|r, w| {
            writeln!(w, "{GREETING}").unwrap();
            expect(r, "qmp_capabilities");
            writeln!(w, r#"{{"return": {{}}}}"#).unwrap();
            expect(r, "bogus");
            writeln!(
                w,
                r#"{{"error": {{"class": "CommandNotFound", "desc": "The command bogus has not been found"}}}}"#
            )
            .unwrap();
        });
        let e = call(&sock, "bogus").unwrap_err();
        assert!(e.to_string().contains("has not been found"), "{e}");
        fs::remove_file(sock).unwrap();
    }

    fn io_kind(e: &anyhow::Error) -> Option<io::ErrorKind> {
        e.chain().find_map(|c| c.downcast_ref::<io::Error>()).map(io::Error::kind)
    }

    #[test]
    fn missing_socket_is_not_found() {
        let e = call(Path::new("/nonexistent/qmp.sock"), "query-status").unwrap_err();
        assert_eq!(io_kind(&e), Some(io::ErrorKind::NotFound));
    }

    #[test]
    fn silent_server_times_out() {
        // QEMU accepts a second client but never greets it while the first is connected.
        let sock = serve(|_, _| thread::sleep(TIMEOUT * 2));
        let e = call(&sock, "query-status").unwrap_err();
        assert_eq!(io_kind(&e), Some(io::ErrorKind::WouldBlock));
        fs::remove_file(sock).unwrap();
    }
}
