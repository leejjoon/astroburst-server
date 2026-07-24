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

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

const DEFAULT_REMOTE_PORT: u16 = 8080;
/// Detect a dead peer within ~45s (15s x 3) instead of leaving a zombie
/// tunnel that only fails at the next client request.
const SERVER_ALIVE_INTERVAL: u32 = 15;
const SERVER_ALIVE_COUNT_MAX: u32 = 3;
const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// How long to wait for the forward to come up after spawning ssh (covers
/// slow auth / 2FA-less key exchange on distant hosts).
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(20);

pub struct ConnectOptions {
    pub target: SshTarget,
    pub remote_port: u16,
    /// `None` = pick a free port automatically.
    pub local_port: Option<u16>,
    pub json: bool,
    pub reconnect: bool,
    /// `None` = unlimited reconnect attempts.
    pub max_retries: Option<u32>,
    /// Launch the remote server if the probe finds nothing listening.
    pub start: bool,
    /// Remote binary (path or $PATH name) used by `--start`.
    pub remote_bin: String,
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
            "--help" | "-h" => {
                bail!(
                    "usage: astroburst-server connect <ssh-target> \
                     [--remote-port N] [--local-port N] [--json] \
                     [--no-reconnect] [--max-retries N] [--start] [--remote-bin PATH]\n\
                     <ssh-target>: an ~/.ssh/config alias/hostname, or ssh://user@host[:sshport]"
                );
            }
            other if target.is_none() => target = Some(parse_target(other)?),
            other => bail!("unexpected argument '{other}'"),
        }
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

fn spawn_tunnel(opts: &ConnectOptions, local_port: u16) -> Result<Child> {
    let args = build_ssh_args(&opts.target, local_port, opts.remote_port);
    let mut cmd = Command::new("ssh");
    cmd.args(&args)
        .stdin(Stdio::null())
        // ssh chatter (banners, keepalive errors) belongs on stderr, never
        // polluting --json stdout.
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
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
    cmd.spawn().context("failed to spawn ssh (is OpenSSH installed?)")
}

/// Launch the remote server over ssh (`--start`), detached via nohup.
fn start_remote_server(opts: &ConnectOptions) -> Result<()> {
    eprintln!(
        "connect: starting remote server ({} on {})...",
        opts.remote_bin, opts.target.destination
    );
    let bind = format!("127.0.0.1:{}", opts.remote_port);
    let remote_cmd = format!(
        "nohup env ASTROBURST_BIND={bind} {} >/dev/null 2>&1 & sleep 0.5",
        shell_quote(&opts.remote_bin)
    );
    let mut args: Vec<String> = vec!["-o".into(), "BatchMode=yes".into()];
    if let Some(p) = opts.target.ssh_port {
        args.push("-p".into());
        args.push(p.to_string());
    }
    args.push(opts.target.destination.clone());
    args.push(remote_cmd);
    let status = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run remote start command")?;
    if !status.success() {
        bail!("remote start command exited with {status}");
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
        Some(p) => p,
        None => pick_free_port()?,
    };
    let url = format!("http://127.0.0.1:{local_port}");

    let mut attempt: u32 = 0;
    let mut backoff = BACKOFF_INITIAL;
    let mut announced = false;

    loop {
        attempt += 1;
        let mut child = spawn_tunnel(&opts, local_port)?;

        let mut health = await_health(local_port, Instant::now() + TUNNEL_READY_TIMEOUT);

        if matches!(health, ProbeResult::RemoteNotListening) && opts.start {
            start_remote_server(&opts)?;
            health = await_health(local_port, Instant::now() + TUNNEL_READY_TIMEOUT);
        }

        match &health {
            ProbeResult::Healthy(body) => {
                // Success resets the backoff schedule.
                backoff = BACKOFF_INITIAL;
                if !announced {
                    announced = true;
                    if opts.json {
                        // Single machine-readable line on stdout; everything
                        // else in this program goes to stderr.
                        println!(
                            "{{\"url\":\"{url}\",\"local_port\":{local_port},\"target\":\"{}\",\"health\":{body}}}",
                            opts.target.destination
                        );
                        use std::io::Write as _;
                        let _ = std::io::stdout().flush();
                    } else {
                        eprintln!("connect: tunnel up -- server URL: {url}");
                        eprintln!("connect: remote /health: {body}");
                    }
                } else {
                    eprintln!("connect: tunnel re-established on {url}");
                }
            }
            ProbeResult::RemoteNotListening => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "tunnel to {} is up, but nothing answers on remote 127.0.0.1:{} \
                     (is astroburst-server running there? try --start)",
                    opts.target.destination,
                    opts.remote_port
                );
            }
            ProbeResult::TunnelNotUp => {
                let _ = child.kill();
                let _ = child.wait();
                if !opts.reconnect || opts.max_retries.is_some_and(|m| attempt > m) {
                    bail!(
                        "could not establish tunnel to {} within {TUNNEL_READY_TIMEOUT:?} \
                         (check `ssh {}` works non-interactively)",
                        opts.target.destination,
                        opts.target.destination
                    );
                }
                eprintln!(
                    "connect: tunnel failed to come up (attempt {attempt}); retrying in {backoff:?}"
                );
                std::thread::sleep(backoff);
                backoff = next_backoff(backoff);
                continue;
            }
        }

        // Tunnel healthy: block until the ssh child exits (network drop,
        // keepalive timeout, remote reboot, ...).
        let status = child.wait().context("waiting on ssh child failed")?;
        if !opts.reconnect {
            bail!("ssh tunnel exited ({status}); reconnect disabled");
        }
        if opts.max_retries.is_some_and(|m| attempt >= m) {
            bail!("ssh tunnel exited ({status}); retry budget exhausted");
        }
        eprintln!("connect: tunnel dropped ({status}); reconnecting in {backoff:?}");
        std::thread::sleep(backoff);
        backoff = next_backoff(backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
