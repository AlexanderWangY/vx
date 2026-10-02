//! A synchronous QMP client.
//!
//! QEMU serves one QMP client at a time, so every call is its own short session:
//! connect, read the greeting, negotiate capabilities, run one command, disconnect.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(2);

/// Run `cmd` and return its `return` value. Connection failures keep their `io::Error`,
/// so callers can tell "nothing listening" from "busy".
pub fn call(sock: &Path, cmd: &str) -> Result<Value> {
    call_with(sock, cmd, Value::Null)
}

/// Run `cmd` with `args` (a JSON object, or null for none).
pub fn call_with(sock: &Path, cmd: &str, args: Value) -> Result<Value> {
    let stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut q = Session { r: BufReader::new(stream.try_clone()?), w: stream };
    let greeting = q.read()?;
    ensure!(greeting.get("QMP").is_some(), "{} is not a QMP socket", sock.display());
    q.exec("qmp_capabilities", Value::Null)?;
    q.exec(cmd, args)
}

/// Run a command that starts a background job, such as `snapshot-save`, and wait for the job
/// to end. Each check is its own short session, so the dashboard's polling gets a turn.
pub fn job(sock: &Path, cmd: &str, mut args: Value) -> Result<()> {
    let id = format!("vx-{}", std::process::id());
    args["job-id"] = id.clone().into();
    call_with(sock, cmd, args)?;
    loop {
        let jobs = call(sock, "query-jobs")?;
        let Some(job) = jobs.as_array().and_then(|jobs| jobs.iter().find(|j| j["id"] == id.as_str())) else {
            return Ok(()); // dismissed on its own once it succeeded
        };
        if job["status"] == "concluded" {
            let error = job["error"].as_str().map(String::from);
            let _ = call_with(sock, "job-dismiss", json!({ "id": id }));
            return match error {
                Some(e) => bail!("{e}"),
                None => Ok(()),
            };
        }
        thread::sleep(Duration::from_millis(100));
    }
}

struct Session {
    r: BufReader<UnixStream>,
    w: UnixStream,
}

impl Session {
    fn exec(&mut self, cmd: &str, args: Value) -> Result<Value> {
        let msg = if args.is_null() { json!({ "execute": cmd }) } else { json!({ "execute": cmd, "arguments": args }) };
        writeln!(self.w, "{msg}")?;
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

    type Script = Box<dyn FnOnce(&mut BufReader<UnixStream>, &mut UnixStream) + Send>;

    /// A fake QEMU that serves one connection per script, in order.
    fn serve_all(scripts: Vec<Script>) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let sock = env::temp_dir().join(format!("vx-qmpj-{}-{n}.sock", std::process::id()));
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        thread::spawn(move || {
            for script in scripts {
                let (mut w, _) = listener.accept().unwrap();
                let mut r = BufReader::new(w.try_clone().unwrap());
                writeln!(w, "{GREETING}").unwrap();
                expect(&mut r, "qmp_capabilities");
                writeln!(w, r#"{{"return": {{}}}}"#).unwrap();
                script(&mut r, &mut w);
            }
        });
        sock
    }

    #[test]
    fn jobs_are_waited_for_and_their_errors_reported() {
        let id = format!("vx-{}", std::process::id());
        let jobs = |status: &str, error: Option<&str>| {
            let mut job = serde_json::json!({ "id": id, "status": status });
            if let Some(e) = error {
                job["error"] = e.into();
            }
            serde_json::json!({ "return": [job] }).to_string()
        };
        let (running, failed) = (jobs("running", None), jobs("concluded", Some("Snapshot 'x' does not exist")));
        let sock = serve_all(vec![
            Box::new(|r, w| {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let msg: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(msg["execute"], "snapshot-load");
                assert_eq!(msg["arguments"]["tag"], "x");
                assert!(msg["arguments"]["job-id"].as_str().unwrap().starts_with("vx-"));
                writeln!(w, r#"{{"return": {{}}}}"#).unwrap();
            }),
            Box::new(move |r, w| {
                expect(r, "query-jobs");
                writeln!(w, "{running}").unwrap();
            }),
            Box::new(move |r, w| {
                expect(r, "query-jobs");
                writeln!(w, "{failed}").unwrap();
            }),
            Box::new(|r, w| {
                expect(r, "job-dismiss");
                writeln!(w, r#"{{"return": {{}}}}"#).unwrap();
            }),
        ]);
        let e = job(&sock, "snapshot-load", serde_json::json!({ "tag": "x" })).unwrap_err();
        assert_eq!(e.to_string(), "Snapshot 'x' does not exist");
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
