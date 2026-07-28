//! `astroburst-server tui [URL]` / `astroburst-server connect <target> --tui`
//! — a live dashboard over the Phase-0 observability endpoints (issue #3).
//!
//! Threads:
//! - **UI loop** (this thread): drains poller snapshots + tunnel events,
//!   handles keys, redraws at ~10 fps max.
//! - **Poller**: fetches `/health`, `/v2/sessions`, and the selected
//!   session's `/images` + `/history` every second (or immediately when the
//!   UI pokes it after a selection change / `r`).
//! - **Tunnel supervisor** (`connect --tui` only): `connect::supervise` with
//!   its events routed here instead of stderr — nothing may print once the
//!   alternate screen is up.

mod app;
mod client;
mod ui;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::connect::{self, ConnectOptions, TunnelEvent};
use app::{Action, App, Detail, Snapshot, TunnelState};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_URL: &str = "http://127.0.0.1:8097";

/// Entry for the standalone `tui` subcommand.
pub fn run_standalone(args: &[String]) -> Result<()> {
    let mut url: Option<String> = None;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => bail!(
                "usage: astroburst-server tui [URL]\n\
                 URL: server base URL (default {DEFAULT_URL})\n\
                 For remote servers use: astroburst-server connect <ssh-target> --tui"
            ),
            other if url.is_none() && !other.starts_with('-') => {
                if other.starts_with("ssh://") {
                    bail!("for ssh targets use: astroburst-server connect {other} --tui");
                }
                url = Some(other.trim_end_matches('/').to_string());
            }
            other => bail!("unexpected argument '{other}' (try --help)"),
        }
    }
    run(url.unwrap_or_else(|| DEFAULT_URL.into()), None, None)
}

/// Entry for `connect <target> --tui`: spawn the tunnel supervisor with its
/// events channelled into the dashboard.
pub fn run_connect(opts: ConnectOptions, local_port: u16) -> Result<()> {
    let url = format!("http://127.0.0.1:{local_port}");
    let destination = opts.target.destination.clone();
    let (tunnel_tx, tunnel_rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Fatal outcomes arrive in the UI as TunnelEvent::Fatal; the Err
        // itself has nowhere else to go.
        let _ = connect::supervise(&opts, local_port, &tunnel_tx);
    });
    run(url, Some(TunnelState::new(destination)), Some(tunnel_rx))
}

/// What the poller needs to know each cycle, shared with the UI thread.
struct PollControl {
    selected: Mutex<Option<String>>,
    wake: Sender<()>,
}

fn run(
    base_url: String,
    tunnel: Option<TunnelState>,
    tunnel_rx: Option<Receiver<TunnelEvent>>,
) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        bail!("--tui requires a terminal (stdout is not a tty)");
    }

    // Poller thread: wake channel doubles as the 1 s scheduler (a message =
    // refresh now, timeout = scheduled refresh).
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    let (snap_tx, snap_rx) = mpsc::channel::<Snapshot>();
    let control = Arc::new(PollControl { selected: Mutex::new(None), wake: wake_tx });
    {
        let control = Arc::clone(&control);
        let base = base_url.clone();
        std::thread::spawn(move || loop {
            let selected = control.selected.lock().unwrap().clone();
            if snap_tx.send(poll_once(&base, selected.as_deref())).is_err() {
                return; // UI gone
            }
            match wake_rx.recv_timeout(POLL_INTERVAL) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        });
    }

    // `ratatui::init` enters the alternate screen + raw mode and installs a
    // panic hook that restores the terminal before the panic message prints.
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, base_url, tunnel, tunnel_rx, snap_rx, &control);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    base_url: String,
    tunnel: Option<TunnelState>,
    tunnel_rx: Option<Receiver<TunnelEvent>>,
    snap_rx: Receiver<Snapshot>,
    control: &PollControl,
) -> Result<()> {
    let mut app = App::new(base_url, tunnel);

    loop {
        // 1. Tunnel + poller updates (non-blocking).
        if let Some(rx) = &tunnel_rx {
            for ev in rx.try_iter() {
                app.apply_tunnel(ev);
            }
        }
        for snap in snap_rx.try_iter() {
            app.apply(snap);
        }
        // Keep the poller pointed at the current selection.
        *control.selected.lock().unwrap() = app.selected_sid.clone();

        // 2. Draw.
        terminal.draw(|f| ui::draw(f, &app))?;

        // 3. Input (also paces redraws while idle).
        if event::poll(Duration::from_millis(100)).context("terminal event poll failed")? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match app.on_key(&normalize_key(key.code, key.modifiers)) {
                        Action::None => {}
                        Action::Refresh => {
                            let _ = control.wake.send(());
                        }
                        Action::DeleteSession(sid) => {
                            // Quick loopback call; an error surfaces in the header.
                            if let Err(e) =
                                client::delete(&app.base_url, &format!("/v2/sessions/{sid}"))
                            {
                                app.error = Some(format!("delete failed: {e}"));
                            }
                            let _ = control.wake.send(());
                        }
                    }
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

/// Map crossterm key codes onto the normalized names `App::on_key` speaks.
fn normalize_key(code: KeyCode, modifiers: KeyModifiers) -> String {
    match code {
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => "ctrl-c".into(),
        KeyCode::Char(c) => c.to_lowercase().to_string(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::End => "end".into(),
        _ => String::new(),
    }
}

/// One polling cycle: health + session list + (if selected) images/history.
/// Network errors land in `Snapshot::error` — the TUI shows them and keeps
/// running (a reconnecting tunnel looks exactly like this).
fn poll_once(base_url: &str, selected: Option<&str>) -> Snapshot {
    let mut snap = Snapshot::default();

    match client::get_json(base_url, "/health") {
        Ok((json, latency)) => {
            snap.health = serde_json::from_value(json).ok();
            snap.latency = Some(latency);
        }
        Err(e) => {
            snap.error = Some(format!("unreachable: {e}"));
            return snap;
        }
    }

    match client::get_json(base_url, "/v2/sessions") {
        Ok((json, _)) => {
            snap.sessions = serde_json::from_value(json["sessions"].clone()).unwrap_or_default();
        }
        Err(e) => {
            snap.error = Some(format!("list failed: {e}"));
            return snap;
        }
    }

    if let Some(sid) = selected {
        if snap.sessions.iter().any(|s| s.session_id == sid) {
            let mut detail = Detail { session_id: sid.to_string(), ..Default::default() };
            if let Ok((json, _)) = client::get_json(base_url, &format!("/v2/sessions/{sid}/images"))
            {
                detail.images =
                    serde_json::from_value(json["images"].clone()).unwrap_or_default();
            }
            if let Ok((json, _)) =
                client::get_json(base_url, &format!("/v2/sessions/{sid}/history"))
            {
                detail.history =
                    serde_json::from_value(json["events"].clone()).unwrap_or_default();
            }
            snap.detail = Some(detail);
        }
    }

    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_args_default_and_explicit_url() {
        // Can't run the TUI headless, but arg validation must reject early.
        assert!(run_standalone(&["--bogus".into()]).is_err());
        assert!(run_standalone(&["--help".into()]).is_err()); // usage via Err
        let err = run_standalone(&["ssh://olaf1".into()]).unwrap_err();
        assert!(err.to_string().contains("connect ssh://olaf1 --tui"), "{err}");
    }

    #[test]
    fn key_normalization() {
        assert_eq!(normalize_key(KeyCode::Char('J'), KeyModifiers::NONE), "j");
        assert_eq!(
            normalize_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            "ctrl-c"
        );
        assert_eq!(normalize_key(KeyCode::BackTab, KeyModifiers::SHIFT), "backtab");
        assert_eq!(normalize_key(KeyCode::F(5), KeyModifiers::NONE), "");
    }

    #[test]
    fn poll_against_dead_server_reports_error_not_panic() {
        let snap = poll_once("http://127.0.0.1:1", None);
        assert!(snap.error.as_deref().unwrap_or("").contains("unreachable"));
        assert!(snap.sessions.is_empty());
    }
}
