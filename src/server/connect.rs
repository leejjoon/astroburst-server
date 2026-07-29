//! `astroburst-server connect <target>` — app-managed SSH tunneling to a
//! remote (loopback-bound) astroburst-server, with auto-reconnect.
//!
//! Issue #2. Pattern borrowed from herdr / `docker -H ssh://`: exec the
//! **system OpenSSH binary** (never an embedded SSH library), so
//! `~/.ssh/config` aliases, keys, ssh-agent, jump hosts, and 2FA all work
//! with zero code here. The server keeps its secure loopback-only default
//! bind; this subcommand gives clients a working local URL:
//!
//! ```text
//! astroburst-server connect olaf1                       # ssh-config alias
//! astroburst-server connect ssh://user@host.example:4022
//! ```
//!
//! Design points (see the issue for rationale, all learned the hard way):
//! - the local port is picked automatically from the free range and bound
//!   to `127.0.0.1` **explicitly** — never a hardcoded 8080 (collision) and
//!   never `[::1]` (the IPv4/IPv6 loopback split silently routes clients to
//!   the wrong service);
//! - `/health` is probed through the tunnel at connect time, so a dead
//!   remote server is diagnosed immediately, not at the first real request;
//! - the ssh child is supervised: on exit the tunnel is respawned with
//!   exponential backoff **on the same local port**, so the URL handed to
//!   clients stays valid across network blips.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

const DEFAULT_REMOTE_PORT: u16 = 8097;
/// Local end of the tunnel when `--local-port` is omitted — fixed (matching the
/// remote default) so the URL is stable across runs. Pass `--local-port 0` for a
/// random free port instead (e.g. to run several `connect` sessions at once).
const DEFAULT_LOCAL_PORT: u16 = 8097;
/// Detect a dead peer within ~45s (15s x 3) instead of leaving a zombie
/// tunnel that only fails at the next client request.
const SERVER_ALIVE_INTERVAL: u32 = 15;
const SERVER_ALIVE_COUNT_MAX: u32 = 3;
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// How long to wait for the forward to come up after spawning ssh (covers
/// slow auth / 2FA-less key exchange on distant hosts).
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub struct ConnectOptions {
    pub target: SshTarget,
    pub remote_port: u16,
    /// Local end of the tunnel. `None` → `DEFAULT_LOCAL_PORT` (stable across
    /// runs); `Some(0)` → a random free port; `Some(p)` → exactly `p`.
    pub local_port: Option<u16>,
    pub json: bool,
    pub reconnect: bool,
    /// `None` = unlimited reconnect attempts.
    pub max_retries: Option<u32>,
    /// Launch the remote server if the probe finds nothing listening.
    pub start: bool,
    /// Remote binary (path or $PATH name) used by `--start`.
    pub remote_bin: String,
    /// Run the live dashboard instead of the plain CLI supervisor (issue #3).
    pub tui: bool,
}

/// Status stream from the tunnel supervisor. The CLI prints these; the TUI
/// renders them in its Tunnel pane. Nothing in `supervise` writes to
/// stdout/stderr directly — under `--tui` the alternate screen owns the
/// terminal and any stray print (including ssh's own stderr) would corrupt it.
#[derive(Debug)]
pub enum TunnelEvent {
    /// Healthy `/health` through the tunnel. `reestablished` distinguishes
    /// the first announce (the CLI's `--json` line) from a reconnect.
    Up { health: String, reestablished: bool },
    /// `--start` is launching the remote server.
    StartingRemote { bin: String, dest: String },
    /// The tunnel died or failed to come up; next attempt after `backoff`.
    /// `reason` is human-readable and carries the attempt count when relevant.
    Retrying { backoff: Duration, reason: String },
    /// A line of ssh's stderr (banners, auth errors, keepalive timeouts).
    SshLine(String),
    /// The remote server answered `/health` but its version is not compatible
    /// with this client's (`major.minor` differ). Advisory only — the tunnel
    /// still works; the API may not.
    VersionMismatch { local: String, remote: String },
    /// Supervision is over; `supervise` returns Err with the same reason.
    Fatal { reason: String },
}

/// Pull the `version` string out of a `/health` JSON body, if present.
fn extract_health_version(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("version")?.as_str().map(str::to_owned)
}

/// Parse a `major.minor` pair from a semver-ish string (`"0.2.1"` -> `(0, 2)`).
fn major_minor(v: &str) -> Option<(u64, u64)> {
    let mut it = v.trim().split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    Some((major, minor))
}

/// Client/server are considered compatible when their `major.minor` agree.
/// Patch differences are fine; a differing (or unparseable) minor/major is a
/// warning. Pre-1.0 minor bumps can be breaking, so `major.minor` is the right
/// boundary here rather than `major` alone.
fn versions_compatible(local: &str, remote: &str) -> bool {
    match (major_minor(local), major_minor(remote)) {
        (Some(l), Some(r)) => l == r,
        // If either side is unparseable, fall back to an exact-string compare
        // so we don't warn spuriously but still flag a genuine difference.
        _ => local.trim() == remote.trim(),
    }
}

/// An SSH destination: either a bare word delegated entirely to OpenSSH
/// (`~/.ssh/config` alias or hostname) or an explicit `ssh://` URL whose
/// port is the **SSH** port (not the server's HTTP port).
#[derive(Debug, PartialEq)]
pub struct SshTarget {
    pub destination: String,
    pub ssh_port: Option<u16>,
}

pub fn parse_target(raw: &str) -> Result<SshTarget> {
    if let Some(rest) = raw.strip_prefix("ssh://") {
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() {
            bail!("empty ssh:// target");
        }
        // Split an optional :port off the host part (after any user@).
        let (userhost, port) = match rest.rsplit_once(':') {
            Some((head, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                let port: u16 = p.parse().with_context(|| format!("invalid ssh port '{p}'"))?;
                (head, Some(port))
            }
            _ => (rest, None),
        };
        if userhost.is_empty() {
            bail!("empty host in ssh:// target");
        }
        Ok(SshTarget { destination: userhost.to_string(), ssh_port: port })
    } else if raw.is_empty() || raw.starts_with('-') {
        bail!("invalid ssh target '{raw}'");
    } else {
        Ok(SshTarget { destination: raw.to_string(), ssh_port: None })
    }
}

pub fn parse_args(args: &[String]) -> Result<ConnectOptions> {
    let mut target: Option<SshTarget> = None;
    let mut remote_port = DEFAULT_REMOTE_PORT;
    let mut local_port = None;
    let mut json = false;
    let mut reconnect = true;
    let mut max_retries = None;
    let mut start = false;
    let mut remote_bin = "astroburst-server".to_string();
    let mut tui = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut take_value = |name: &str| -> Result<String> {
            it.next()
                .cloned()
                .with_context(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "--remote-port" => {
                remote_port = take_value("--remote-port")?.parse().context("invalid --remote-port")?
            }
            "--local-port" => {
                local_port =
                    Some(take_value("--local-port")?.parse().context("invalid --local-port")?)
            }
            "--json" => json = true,
            "--no-reconnect" => reconnect = false,
            "--max-retries" => {
                max_retries =
                    Some(take_value("--max-retries")?.parse().context("invalid --max-retries")?)
            }
            "--start" => start = true,
            "--remote-bin" => remote_bin = take_value("--remote-bin")?,
            "--tui" => tui = true,
            "--help" | "-h" => {
                bail!(
                    "usage: astroburst-server connect <ssh-target> \
                     [--remote-port N] [--local-port N] [--json] [--tui] \
                     [--no-reconnect] [--max-retries N] [--start] [--remote-bin PATH]\n\
                     <ssh-target>: an ~/.ssh/config alias/hostname, or ssh://user@host[:sshport]\n\
                     --local-port defaults to {DEFAULT_LOCAL_PORT}; pass 0 for a random free port"
                );
            }
            other if target.is_none() => target = Some(parse_target(other)?),
            other => bail!("unexpected argument '{other}'"),
        }
    }

    if json && tui {
        bail!("--json and --tui are mutually exclusive");
    }

    Ok(ConnectOptions {
        target: target.context("missing ssh target (try --help)")?,
        remote_port,
        local_port,
        json,
        reconnect,
        max_retries,
        start,
        remote_bin,
        tui,
    })
}

/// The ssh argv (excluding the program name) for the tunnel process.
pub fn build_ssh_args(target: &SshTarget, local_port: u16, remote_port: u16) -> Vec<String> {
    let mut args = vec![
        "-N".to_string(),
        "-L".to_string(),
        // 127.0.0.1 on BOTH ends, explicitly: local so clients on the
        // v4 loopback always reach us; remote because the server's secure
        // default bind is loopback-only.
        format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        "-o".to_string(),
        format!("ServerAliveInterval={SERVER_ALIVE_INTERVAL}"),
        "-o".to_string(),
        format!("ServerAliveCountMax={SERVER_ALIVE_COUNT_MAX}"),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
    ];
    if let Some(p) = target.ssh_port {
        args.push("-p".to_string());
        args.push(p.to_string());
    }
    args.push(target.destination.clone());
    args
}

/// Pick a free port on the v4 loopback by binding port 0. The listener is
/// dropped before ssh binds it — a tiny TOCTOU window, accepted because
/// `ExitOnForwardFailure=yes` turns a lost race into a visible respawn.
pub fn pick_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("failed to probe for a free port")?;
    Ok(listener.local_addr()?.port())
}

/// Outcome of one `/health` probe attempt through the tunnel.
#[derive(Debug, PartialEq)]
pub enum ProbeResult {
    /// HTTP 200 with the health JSON body.
    Healthy(String),
    /// TCP connect to the local port failed — the tunnel itself isn't up
    /// (ssh still authenticating, or the child died).
    TunnelNotUp,
    /// The tunnel accepted but the stream died / returned garbage — with
    /// `-L` forwarding this is what a connection-refused on the REMOTE side
    /// looks like locally: the remote server isn't listening.
    RemoteNotListening,
}

pub fn probe_health(local_port: u16) -> ProbeResult {
    let addr = format!("127.0.0.1:{local_port}");
    let mut stream = match TcpStream::connect_timeout(
        &addr.parse().expect("loopback addr parses"),
        Duration::from_millis(1000),
    ) {
        Ok(s) => s,
        Err(_) => return ProbeResult::TunnelNotUp,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let request = format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{local_port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return ProbeResult::RemoteNotListening;
    }
    let mut response = Vec::new();
    if stream.read_to_end(&mut response).is_err() || response.is_empty() {
        return ProbeResult::RemoteNotListening;
    }
    let text = String::from_utf8_lossy(&response);
    if !text.starts_with("HTTP/1.1 200") && !text.starts_with("HTTP/1.0 200") {
        return ProbeResult::RemoteNotListening;
    }
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.trim().to_string())
        .unwrap_or_default();
    ProbeResult::Healthy(body)
}

/// Wait for the tunnel to come up and the remote server to answer
/// `/health`, polling until `deadline`. Returns the health body, or the
/// last non-healthy state at timeout.
fn await_health(local_port: u16, deadline: Instant) -> ProbeResult {
    let mut last = ProbeResult::TunnelNotUp;
    while Instant::now() < deadline {
        match probe_health(local_port) {
            ProbeResult::Healthy(body) => return ProbeResult::Healthy(body),
            other => last = other,
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    last
}

fn spawn_tunnel(
    opts: &ConnectOptions,
    local_port: u16,
    events: &mpsc::Sender<TunnelEvent>,
) -> Result<Child> {
    let args = build_ssh_args(&opts.target, local_port, opts.remote_port);
    let mut cmd = Command::new("ssh");
    cmd.args(&args)
        .stdin(Stdio::null())
        // ssh chatter (banners, keepalive errors) is captured and forwarded
        // as SshLine events — never printed directly, so it can't pollute
        // --json stdout or the --tui alternate screen.
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // Terminal Ctrl-C reaches the child via the foreground process group,
    // but a `kill <supervisor>` from elsewhere would orphan the ssh child
    // and leak the tunnel. PR_SET_PDEATHSIG ties the child's lifetime to
    // ours at the kernel level.
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("failed to spawn ssh (is OpenSSH installed?)")?;

    // Forward ssh's stderr line-by-line; the thread ends at child exit (EOF).
    if let Some(stderr) = child.stderr.take() {
        let events = events.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                // Drop the per-channel "connection refused" spam: ssh emits one
                // of these for every forwarded channel while the remote server
                // isn't listening, so a single stuck run prints dozens. The
                // condition is already diagnosed cleanly as `RemoteNotListening`
                // (see supervise), so forwarding the raw lines is pure noise.
                if is_channel_refused_noise(&line) {
                    continue;
                }
                if events.send(TunnelEvent::SshLine(line)).is_err() {
                    break;
                }
            }
        });
    }
    Ok(child)
}

/// True for ssh's forwarded-channel refusal line, e.g.
/// `channel 3: open failed: connect failed: Connection refused`. These mean
/// "the remote end of the -L forward refused the connection" — i.e. the remote
/// server isn't listening on `--remote-port`. We detect and report that state
/// via the health probe, so the raw lines are redundant noise worth dropping.
fn is_channel_refused_noise(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with("channel ")
        && l.contains("open failed")
        && l.contains("Connection refused")
}

/// Launch the remote server over ssh (`--start`), detached via nohup.
///
/// The remote command is written to be self-diagnosing so that a failed start
/// produces an actionable message rather than a silent no-op followed by the
/// `RemoteNotListening` flood:
///   - it first checks the binary is resolvable (`command -v`, or an executable
///     absolute path) and exits 127 with a hint if not — this catches the most
///     common failure, a binary that isn't on the non-login SSH `$PATH`;
///   - it redirects the server's own stdout/stderr to a per-port log file
///     (not `/dev/null`) so bind failures, panics, etc. are preserved;
///   - after a short wait it confirms the process is still alive; if it died,
///     it echoes the tail of that log so the real reason travels back to us.
/// Any line the remote script prints to stderr is forwarded as an `SshLine`.
fn start_remote_server(opts: &ConnectOptions, events: &mpsc::Sender<TunnelEvent>) -> Result<()> {
    let _ = events.send(TunnelEvent::StartingRemote {
        bin: opts.remote_bin.clone(),
        dest: opts.target.destination.clone(),
    });
    let bind = format!("127.0.0.1:{}", opts.remote_port);
    let qbin = shell_quote(&opts.remote_bin);
    let port = opts.remote_port;
    // A POSIX-sh remote script (see doc comment). `log` lands in $TMPDIR or /tmp.
    let remote_cmd = format!(
        r#"bin={qbin}; log="${{TMPDIR:-/tmp}}/astroburst-server-{port}.log"; \
if ! command -v "$bin" >/dev/null 2>&1 && [ ! -x "$bin" ]; then \
  echo "astroburst-connect: remote binary '$bin' not found on PATH and is not an executable path" >&2; \
  echo "astroburst-connect: pass --remote-bin with an absolute path (a non-login ssh shell often omits ~/.cargo/bin, ~/.local/bin)" >&2; \
  exit 127; \
fi; \
nohup env ASTROBURST_BIND={bind} "$bin" >"$log" 2>&1 & \
pid=$!; sleep 1; \
if ! kill -0 "$pid" 2>/dev/null; then \
  echo "astroburst-connect: remote server exited immediately (see $log on {dest}); last lines:" >&2; \
  tail -n 20 "$log" >&2 2>/dev/null || true; \
  exit 1; \
fi; \
echo "astroburst-connect: remote server started (pid $pid, logging to $log)" >&2"#,
        dest = opts.target.destination,
    );
    let mut args: Vec<String> = vec!["-o".into(), "BatchMode=yes".into()];
    if let Some(p) = opts.target.ssh_port {
        args.push("-p".into());
        args.push(p.to_string());
    }
    args.push(opts.target.destination.clone());
    args.push(remote_cmd);
    let output = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run remote start command")?;
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        let _ = events.send(TunnelEvent::SshLine(line.to_string()));
    }
    if !output.status.success() {
        // The forwarded stderr above already carries the specific reason
        // (binary-not-found / crash-log tail); keep this terse.
        bail!(
            "--start failed to bring up astroburst-server on {} (see messages above)",
            opts.target.destination
        );
    }
    Ok(())
}

/// Minimal single-token shell quoting for the remote command line.
fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Exponential backoff schedule: 1s, 2s, 4s, ... capped at 30s.
pub fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(BACKOFF_CAP)
}

/// Entry point for `astroburst-server connect ...`. Runs until Ctrl-C (the
/// foreground process group delivers SIGINT to the ssh child too, tearing
/// the tunnel down with us) or until the retry budget is exhausted.
pub fn run(args: &[String]) -> Result<()> {
    let opts = parse_args(args)?;
    let local_port = match opts.local_port {
        // `--local-port 0` explicitly opts into a random free port.
        Some(0) => pick_free_port()?,
        Some(p) => p,
        // Omitted → fixed default, so the local URL is stable across runs.
        None => DEFAULT_LOCAL_PORT,
    };

    if opts.tui {
        return crate::tui::run_connect(opts, local_port);
    }

    // CLI mode: a printer thread renders supervisor events exactly as the
    // pre-refactor code did — one machine-readable line on stdout under
    // --json, everything else on stderr.
    let (tx, rx) = mpsc::channel();
    let url = format!("http://127.0.0.1:{local_port}");
    let json = opts.json;
    let destination = opts.target.destination.clone();
    let printer = std::thread::spawn(move || {
        for event in rx {
            match event {
                TunnelEvent::Up { health, reestablished: false } => {
                    if json {
                        println!(
                            "{{\"url\":\"{url}\",\"local_port\":{local_port},\"target\":\"{destination}\",\"health\":{health}}}"
                        );
                        use std::io::Write as _;
                        let _ = std::io::stdout().flush();
                    } else {
                        eprintln!("connect: tunnel up -- server URL: {url}");
                        eprintln!("connect: remote /health: {health}");
                    }
                }
                TunnelEvent::Up { reestablished: true, .. } => {
                    eprintln!("connect: tunnel re-established on {url}");
                }
                TunnelEvent::StartingRemote { bin, dest } => {
                    eprintln!("connect: starting remote server ({bin} on {dest})...");
                }
                TunnelEvent::Retrying { backoff, reason, .. } => {
                    eprintln!("connect: {reason}; reconnecting in {backoff:?}");
                }
                TunnelEvent::SshLine(line) => eprintln!("{line}"),
                TunnelEvent::VersionMismatch { local, remote } => {
                    // Always to stderr — keeps the --json stdout line clean.
                    eprintln!(
                        "connect: WARNING remote astroburst-server version {remote} is \
                         incompatible with this client {local} (major.minor differ); the \
                         tunnel works but the API may not — rebuild/redeploy to match."
                    );
                }
                TunnelEvent::Fatal { .. } => {} // reported via supervise's Err
            }
        }
    });

    let result = supervise(&opts, local_port, &tx);
    drop(tx);
    let _ = printer.join();
    result
}

/// The supervision loop: spawn the tunnel, await health, block on the ssh
/// child, respawn with exponential backoff on the same local port. Status
/// goes out as [`TunnelEvent`]s; the function only returns on a fatal
/// condition (its `Err` mirrors the final `Fatal` event).
pub fn supervise(
    opts: &ConnectOptions,
    local_port: u16,
    events: &mpsc::Sender<TunnelEvent>,
) -> Result<()> {
    // A fatal condition is both an event (for the TUI pane) and an Err (for
    // the CLI exit path).
    macro_rules! fatal {
        ($($arg:tt)*) => {{
            let reason = format!($($arg)*);
            let _ = events.send(TunnelEvent::Fatal { reason: reason.clone() });
            bail!(reason);
        }};
    }

    let mut attempt: u32 = 0;
    let mut backoff = BACKOFF_INITIAL;
    let mut announced = false;

    loop {
        attempt += 1;
        let mut child = spawn_tunnel(opts, local_port, events)?;

        let mut health = await_health(local_port, Instant::now() + TUNNEL_READY_TIMEOUT);

        if matches!(health, ProbeResult::RemoteNotListening) && opts.start {
            if let Err(e) = start_remote_server(opts, events) {
                let _ = child.kill();
                let _ = child.wait();
                fatal!("{e}");
            }
            health = await_health(local_port, Instant::now() + TUNNEL_READY_TIMEOUT);
        }

        match &health {
            ProbeResult::Healthy(body) => {
                // Success resets the backoff schedule.
                backoff = BACKOFF_INITIAL;
                let first_announce = !announced;
                let _ = events.send(TunnelEvent::Up {
                    health: body.clone(),
                    reestablished: announced,
                });
                announced = true;
                // Verify the remote build is compatible — only on the first
                // announce, so reconnects to the same server don't re-warn.
                if first_announce {
                    let local = env!("CARGO_PKG_VERSION");
                    if let Some(remote) = extract_health_version(body) {
                        if !versions_compatible(local, &remote) {
                            let _ = events.send(TunnelEvent::VersionMismatch {
                                local: local.to_string(),
                                remote,
                            });
                        }
                    }
                }
            }
            ProbeResult::RemoteNotListening => {
                let _ = child.kill();
                let _ = child.wait();
                if opts.start {
                    // --start ran but the port is still dead: the specific
                    // reason was already forwarded by start_remote_server.
                    fatal!(
                        "the SSH tunnel to {dest} is up, but after --start nothing is \
                         listening on the remote's 127.0.0.1:{port}. The remote server \
                         failed to start or bind (see the astroburst-connect messages \
                         above). Check it starts by hand:  \
                         ssh {dest} 'ASTROBURST_BIND=127.0.0.1:{port} {bin}'",
                        dest = opts.target.destination,
                        port = opts.remote_port,
                        bin = opts.remote_bin,
                    );
                }
                fatal!(
                    "the SSH tunnel to {dest} is up, but nothing is listening on the \
                     remote's 127.0.0.1:{port}. The astroburst-server is not running \
                     there (or is on a different port). Start it with --start, or if it \
                     is already running on another port pass --remote-port N.",
                    dest = opts.target.destination,
                    port = opts.remote_port,
                );
            }
            ProbeResult::TunnelNotUp => {
                let _ = child.kill();
                let _ = child.wait();
                if !opts.reconnect || opts.max_retries.is_some_and(|m| attempt > m) {
                    fatal!(
                        "could not establish tunnel to {} within {TUNNEL_READY_TIMEOUT:?} \
                         (check `ssh {}` works non-interactively)",
                        opts.target.destination,
                        opts.target.destination
                    );
                }
                let _ = events.send(TunnelEvent::Retrying {
                    backoff,
                    reason: format!("tunnel failed to come up (attempt {attempt})"),
                });
                std::thread::sleep(backoff);
                backoff = next_backoff(backoff);
                continue;
            }
        }

        // Tunnel healthy: block until the ssh child exits (network drop,
        // keepalive timeout, remote reboot, ...).
        let status = child.wait().context("waiting on ssh child failed")?;
        if !opts.reconnect {
            fatal!("ssh tunnel exited ({status}); reconnect disabled");
        }
        if opts.max_retries.is_some_and(|m| attempt >= m) {
            fatal!("ssh tunnel exited ({status}); retry budget exhausted");
        }
        let _ = events.send(TunnelEvent::Retrying {
            backoff,
            reason: format!("tunnel dropped ({status})"),
        });
        std::thread::sleep(backoff);
        backoff = next_backoff(backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_version_is_extracted() {
        let body = r#"{"status":"ok","version":"0.2.1","sessions_active":0}"#;
        assert_eq!(extract_health_version(body).as_deref(), Some("0.2.1"));
        assert_eq!(extract_health_version("not json"), None);
        assert_eq!(extract_health_version(r#"{"status":"ok"}"#), None);
    }

    #[test]
    fn version_compatibility_uses_major_minor() {
        // Patch differences are compatible.
        assert!(versions_compatible("0.2.1", "0.2.9"));
        assert!(versions_compatible("1.4.0", "1.4.7"));
        // Minor / major differences are not.
        assert!(!versions_compatible("0.2.1", "0.3.0"));
        assert!(!versions_compatible("0.2.1", "1.2.1"));
        // Unparseable falls back to exact-string compare.
        assert!(versions_compatible("weird", "weird"));
        assert!(!versions_compatible("0.2.1", "weird"));
    }

    #[test]
    fn channel_refused_noise_is_detected() {
        // ssh's real wording, with and without leading whitespace.
        assert!(is_channel_refused_noise(
            "channel 3: open failed: connect failed: Connection refused"
        ));
        assert!(is_channel_refused_noise(
            "  channel 12: open failed: connect failed: Connection refused"
        ));
        // Genuinely useful lines must NOT be filtered.
        assert!(!is_channel_refused_noise("Permission denied (publickey)."));
        assert!(!is_channel_refused_noise(
            "ssh: connect to host olaf1 port 22: Connection refused"
        ));
        assert!(!is_channel_refused_noise(
            "astroburst-connect: remote binary 'astroburst-server' not found on PATH and is not an executable path"
        ));
    }

    #[test]
    fn parse_target_bare_alias() {
        assert_eq!(
            parse_target("olaf1").unwrap(),
            SshTarget { destination: "olaf1".into(), ssh_port: None }
        );
    }

    #[test]
    fn parse_target_ssh_url_with_user_and_port() {
        assert_eq!(
            parse_target("ssh://jjlee@olaf1.ibs.re.kr:4022").unwrap(),
            SshTarget { destination: "jjlee@olaf1.ibs.re.kr".into(), ssh_port: Some(4022) }
        );
    }

    #[test]
    fn parse_target_ssh_url_without_port() {
        assert_eq!(
            parse_target("ssh://user@host").unwrap(),
            SshTarget { destination: "user@host".into(), ssh_port: None }
        );
    }

    #[test]
    fn parse_target_rejects_garbage() {
        assert!(parse_target("").is_err());
        assert!(parse_target("--json").is_err());
        assert!(parse_target("ssh://").is_err());
    }

    #[test]
    fn build_ssh_args_shape() {
        let t = SshTarget { destination: "user@host".into(), ssh_port: Some(4022) };
        let args = build_ssh_args(&t, 18123, 8080);
        let joined = args.join(" ");
        assert!(joined.contains("-N"));
        assert!(joined.contains("-L 127.0.0.1:18123:127.0.0.1:8080"));
        assert!(joined.contains("ExitOnForwardFailure=yes"));
        assert!(joined.contains("ServerAliveInterval=15"));
        assert!(joined.contains("-p 4022"));
        assert!(joined.ends_with("user@host"));
        // The local bind must be the v4 loopback, never [::1] or a bare port.
        assert!(!joined.contains("::1"));
    }

    #[test]
    fn parse_args_full() {
        let args: Vec<String> = [
            "ssh://u@h:2222", "--remote-port", "9090", "--local-port", "17000",
            "--json", "--no-reconnect", "--max-retries", "5", "--start",
            "--remote-bin", "/opt/ab/astroburst-server",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let o = parse_args(&args).unwrap();
        assert_eq!(o.target.destination, "u@h");
        assert_eq!(o.target.ssh_port, Some(2222));
        assert_eq!(o.remote_port, 9090);
        assert_eq!(o.local_port, Some(17000));
        assert!(o.json);
        assert!(!o.reconnect);
        assert_eq!(o.max_retries, Some(5));
        assert!(o.start);
        assert_eq!(o.remote_bin, "/opt/ab/astroburst-server");
    }

    #[test]
    fn parse_args_requires_target() {
        assert!(parse_args(&["--json".to_string()]).is_err());
    }

    #[test]
    fn parse_args_tui_flag_and_json_conflict() {
        let o = parse_args(&["host".to_string(), "--tui".to_string()]).unwrap();
        assert!(o.tui);
        assert!(!parse_args(&["host".to_string()]).unwrap().tui);
        let err =
            parse_args(&["host".to_string(), "--tui".to_string(), "--json".to_string()])
                .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn free_port_is_v4_loopback_bindable() {
        let p = pick_free_port().unwrap();
        assert!(p > 0);
        // Should be immediately re-bindable on the v4 loopback.
        TcpListener::bind(("127.0.0.1", p)).unwrap();
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = BACKOFF_INITIAL;
        let seq: Vec<u64> = (0..7)
            .map(|_| {
                let cur = b.as_secs();
                b = next_backoff(b);
                cur
            })
            .collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 30, 30]);
    }

    #[test]
    fn probe_health_against_mock_server() {
        // Healthy mock.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let body = r#"{"status":"ok","version":"0.2.0"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
        });
        match probe_health(port) {
            ProbeResult::Healthy(body) => assert!(body.contains("\"status\":\"ok\"")),
            other => panic!("expected Healthy, got {other:?}"),
        }

        // Nothing listening -> TunnelNotUp.
        let free = pick_free_port().unwrap();
        assert_eq!(probe_health(free), ProbeResult::TunnelNotUp);

        // Listener that closes without answering -> RemoteNotListening
        // (what a refused remote connection looks like through -L).
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            drop(s);
        });
        assert_eq!(probe_health(port), ProbeResult::RemoteNotListening);
    }
}
