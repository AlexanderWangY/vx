//! Live numbers from inside a running VM, for the dashboard's details pane.
//!
//! One long-lived ssh session runs a small shell loop in the guest that prints its /proc
//! counters every couple of seconds. Nothing is installed in the guest: every image has a
//! POSIX shell, /proc and df. Rates (CPU, network) come from the difference between samples.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::backend;
use crate::ssh;
use crate::vx::Home;

/// How many samples the graphs remember.
pub const HISTORY: usize = 120;
/// Ends each sample, so it can be handed over as soon as it's complete.
const END: &str = "@vx";
/// Every 2 s. Once the connection drops, the next write gets SIGPIPE and the loop ends.
const SCRIPT: &str = "while :; do grep '^cpu' /proc/stat; \
    grep -E '^(MemTotal|MemAvailable|SwapTotal|SwapFree):' /proc/meminfo; \
    df -kP / | tail -n 1; cat /proc/loadavg /proc/uptime /proc/net/dev; echo @vx; sleep 2; done";
/// How long to wait before trying again when ssh fails, e.g. while the VM boots.
const RETRY: Duration = Duration::from_secs(3);

/// One reading of the guest's counters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    /// (busy, total) jiffies since boot: the whole machine first, then each core.
    pub cpus: Vec<(u64, u64)>,
    pub mem_total: u64,
    pub mem_available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    /// Of the root filesystem.
    pub disk_used: u64,
    pub disk_size: u64,
    pub load: [f32; 3],
    /// Seconds since the guest booted.
    pub uptime: f64,
    /// Bytes received and sent on every interface but loopback.
    pub net_rx: u64,
    pub net_tx: u64,
}

impl Sample {
    /// Parse the script's output for one sample. Lines it doesn't recognise are skipped.
    pub fn parse(text: &str) -> Sample {
        let mut s = Sample::default();
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let kb = || fields.get(1).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0) * 1024;
            match fields.first().copied() {
                Some(cpu) if cpu.starts_with("cpu") => {
                    // user nice system idle iowait irq softirq steal; guest time is already in user.
                    let n: Vec<u64> = fields[1..].iter().take(8).filter_map(|f| f.parse().ok()).collect();
                    let total: u64 = n.iter().sum();
                    let idle = n.get(3).unwrap_or(&0) + n.get(4).unwrap_or(&0);
                    s.cpus.push((total.saturating_sub(idle), total));
                }
                Some("MemTotal:") => s.mem_total = kb(),
                Some("MemAvailable:") => s.mem_available = kb(),
                Some("SwapTotal:") => s.swap_total = kb(),
                Some("SwapFree:") => s.swap_free = kb(),
                // df -kP: filesystem, size, used, available, capacity, mount point.
                _ if fields.len() == 6 && fields[5] == "/" => {
                    let n = |i: usize| fields[i].parse::<u64>().unwrap_or(0) * 1024;
                    // Like df's capacity: what's reserved for root doesn't count.
                    s.disk_used = n(2);
                    s.disk_size = n(2) + n(3);
                }
                // /proc/loadavg: 0.42 0.31 0.20 1/123 4567
                _ if fields.len() == 5 && fields[3].contains('/') => {
                    for (i, f) in fields[..3].iter().enumerate() {
                        s.load[i] = f.parse().unwrap_or(0.0);
                    }
                }
                // /proc/uptime: seconds up, seconds idle
                _ if fields.len() == 2 && fields.iter().all(|f| f.parse::<f64>().is_ok()) => {
                    s.uptime = fields[0].parse().unwrap_or(0.0);
                }
                // /proc/net/dev: "  eth0: rx_bytes … (8 fields) tx_bytes …"
                _ => {
                    if let Some((name, counters)) = line.split_once(':')
                        && name.trim() != "lo"
                        && !name.trim().contains(' ')
                    {
                        let n: Vec<u64> = counters.split_whitespace().filter_map(|f| f.parse().ok()).collect();
                        if n.len() >= 16 {
                            s.net_rx += n[0];
                            s.net_tx += n[8];
                        }
                    }
                }
            }
        }
        s
    }
}

/// What the dashboard shows: the latest sample, rates since the one before, and history.
#[derive(Debug, Default)]
pub struct Stats {
    pub latest: Sample,
    /// 0 to 1: the whole machine, then each core.
    pub cpu: f64,
    pub cores: Vec<f64>,
    /// Bytes per second.
    pub rx: f64,
    pub tx: f64,
    /// Oldest first.
    pub cpu_history: VecDeque<f64>,
    pub rx_history: VecDeque<f64>,
    pub tx_history: VecDeque<f64>,
}

impl Stats {
    pub fn push(&mut self, sample: Sample) {
        // The first sample, or the guest rebooted: averages since boot, and no rates yet.
        let first = self.latest.uptime == 0.0 || sample.uptime < self.latest.uptime;
        let before = if first { &[][..] } else { &self.latest.cpus[..] };
        let usage = |i: usize, (busy, total): (u64, u64)| {
            let (b0, t0) = before.get(i).copied().unwrap_or((0, 0));
            let (busy, total) = (busy.saturating_sub(b0), total.saturating_sub(t0));
            if total == 0 { 0.0 } else { busy as f64 / total as f64 }
        };
        let mut all = sample.cpus.iter().enumerate().map(|(i, c)| usage(i, *c));
        self.cpu = all.next().unwrap_or(0.0);
        self.cores = all.collect();
        let seconds = sample.uptime - self.latest.uptime;
        (self.rx, self.tx) = if first || seconds <= 0.0 {
            (0.0, 0.0)
        } else {
            let rate = |now: u64, then: u64| now.saturating_sub(then) as f64 / seconds;
            (rate(sample.net_rx, self.latest.net_rx), rate(sample.net_tx, self.latest.net_tx))
        };
        for (history, value) in
            [(&mut self.cpu_history, self.cpu), (&mut self.rx_history, self.rx), (&mut self.tx_history, self.tx)]
        {
            if history.len() == HISTORY {
                history.pop_front();
            }
            history.push_back(value);
        }
        self.latest = sample;
    }
}

/// Samples one VM on a thread until dropped.
pub struct Watcher {
    name: String,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Watcher {
    /// Call `send` with each sample from `name`, retrying while it can't be reached. Stops on
    /// its own once `send` returns false.
    pub fn start(home: &Path, name: String, send: impl Fn(Sample) -> bool + Send + 'static) -> Watcher {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let watcher = Watcher { name: name.clone(), stop: stop.clone(), child: child.clone() };
        let home = Home::at(home);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if !sample(&home, &name, &stop, &child, &send) {
                    return;
                }
                for _ in 0..30 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(RETRY / 30);
                }
            }
        });
        watcher
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

/// One ssh session, until it ends. Returns false once nobody is listening any more.
fn sample(
    home: &Home,
    name: &str,
    stop: &AtomicBool,
    slot: &Mutex<Option<Child>>,
    send: &impl Fn(Sample) -> bool,
) -> bool {
    let Ok(vm) = home.load(name) else { return true };
    let Ok(backend) = backend::get(&vm.spec.backend) else { return true };
    let config = vm.path("ssh_config");
    // Written by `vx new` and `vx ssh`; only missing for VMs made by an older vx.
    if !config.exists() && ssh::write_config(home, &vm, backend.ssh_addr(&vm)).is_err() {
        return true;
    }
    let spawned = Command::new("ssh")
        .arg("-F")
        .arg(&config)
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"])
        // Notice a VM that went away, rather than waiting on it forever.
        .args(["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2"])
        .arg(ssh::alias(name))
        .arg(SCRIPT)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Its own process group, so Ctrl-C in a foreground ssh session can't end it.
        .process_group(0)
        .spawn();
    let Ok(mut child) = spawned else { return true };
    let Some(stdout) = child.stdout.take() else { return true };
    {
        let mut slot = slot.lock().unwrap();
        // Dropped between the check in the loop and now: don't leave ssh running.
        if stop.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return true;
        }
        *slot = Some(child);
    }
    let mut text = String::new();
    let mut listening = true;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if line != END {
            text.push_str(&line);
            text.push('\n');
            continue;
        }
        if !send(Sample::parse(&text)) {
            listening = false;
            break;
        }
        text.clear();
    }
    if let Some(mut child) = slot.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    listening
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: &str = "\
cpu  100 0 50 800 50 0 0 0 0 0
cpu0 60 0 20 400 20 0 0 0 0 0
cpu1 40 0 30 400 30 0 0 0 0 0
MemTotal:        4000000 kB
MemAvailable:    3000000 kB
SwapTotal:       1000000 kB
SwapFree:         900000 kB
/dev/vda3       19000000  4000000  15000000      22% /
0.42 0.31 0.20 1/123 4567
1234.50 4000.00
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:    5000      50    0    0    0     0          0         0     5000      50    0    0    0     0       0          0
  eth0: 1000000    900    0    0    0     0          0         0   200000    800    0    0    0     0       0          0
";

    #[test]
    fn parses_proc_output() {
        let s = Sample::parse(OUTPUT);
        assert_eq!(s.cpus, vec![(150, 1000), (80, 500), (70, 500)]);
        assert_eq!((s.mem_total, s.mem_available), (4_096_000_000, 3_072_000_000));
        assert_eq!((s.swap_total, s.swap_free), (1_024_000_000, 921_600_000));
        assert_eq!((s.disk_used, s.disk_size), (4_096_000_000, 19_456_000_000));
        assert_eq!(s.load, [0.42, 0.31, 0.20]);
        assert_eq!(s.uptime, 1234.5);
        assert_eq!((s.net_rx, s.net_tx), (1_000_000, 200_000));
    }

    #[test]
    fn rates_come_from_the_difference() {
        let mut stats = Stats::default();
        let first = Sample::parse(OUTPUT);
        stats.push(first.clone());
        // Since boot, until there's a second sample.
        assert_eq!(stats.cpu, 0.15);
        assert_eq!((stats.rx, stats.tx), (0.0, 0.0));

        let next = Sample {
            cpus: vec![(250, 1200), (180, 600), (70, 600)],
            uptime: first.uptime + 2.0,
            net_rx: first.net_rx + 4000,
            net_tx: first.net_tx + 1000,
            ..first
        };
        stats.push(next);
        assert_eq!(stats.cpu, 0.5);
        assert_eq!(stats.cores, vec![1.0, 0.0]);
        assert_eq!((stats.rx, stats.tx), (2000.0, 500.0));
        assert_eq!(stats.cpu_history, [0.15, 0.5]);
    }

    #[test]
    fn a_reboot_starts_over() {
        let mut stats = Stats::default();
        stats.push(Sample::parse(OUTPUT));
        let rebooted = Sample { uptime: 3.0, cpus: vec![(1, 10)], net_rx: 10, ..Sample::default() };
        stats.push(rebooted);
        assert_eq!(stats.cpu, 0.1);
        assert_eq!(stats.rx, 0.0);
    }

    #[test]
    fn history_is_bounded() {
        let mut stats = Stats::default();
        for i in 1..=HISTORY + 5 {
            stats.push(Sample { uptime: i as f64, ..Sample::default() });
        }
        assert_eq!(stats.cpu_history.len(), HISTORY);
    }
}
