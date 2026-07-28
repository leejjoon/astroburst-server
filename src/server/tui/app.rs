//! TUI application state and its (pure, terminal-free) update logic.
//!
//! Everything here is plain data + transitions so it unit-tests without a
//! terminal: snapshots from the poller thread are `apply()`d, tunnel events
//! from the `connect` supervisor are `apply_tunnel()`d, and keystrokes are
//! interpreted by `on_key()`. Rendering lives in `ui.rs`.

use std::collections::VecDeque;
use std::time::Duration;

use serde::Deserialize;

use crate::connect::TunnelEvent;

/// Cap on retained tunnel-log lines (ssh stderr + status transitions).
const TUNNEL_LOG_CAP: usize = 200;

// ── data fetched from the server ─────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Health {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub io_mode: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    #[serde(default)]
    pub created_unix: u64,
    #[serde(default)]
    pub idle_secs: u64,
    #[serde(default)]
    pub active_ref: Option<String>,
    #[serde(default)]
    pub image_count: usize,
    #[serde(default)]
    pub cache_bytes: u64,
    #[serde(default)]
    pub running_jobs: usize,
    #[serde(default)]
    pub last_seq: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageEntry {
    pub image_ref: String,
    #[serde(default)]
    pub width: usize,
    #[serde(default)]
    pub height: usize,
    #[serde(default)]
    pub hdu: Option<usize>,
    #[serde(default)]
    pub extname: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HistoryEvent {
    pub seq: u64,
    #[serde(default)]
    pub unix_ms: u64,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub image_ref: Option<String>,
    #[serde(default)]
    pub status: u16,
    #[serde(default)]
    pub duration_ms: u64,
}

/// Everything the poller fetched about the currently-selected session.
#[derive(Debug, Clone, Default)]
pub struct Detail {
    pub session_id: String,
    pub images: Vec<ImageEntry>,
    pub history: Vec<HistoryEvent>,
}

/// One poll cycle's worth of server state.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// `None` when the server was unreachable — `error` says why.
    pub health: Option<Health>,
    pub latency: Option<Duration>,
    pub sessions: Vec<SessionSummary>,
    pub detail: Option<Detail>,
    pub error: Option<String>,
}

// ── tunnel state (connect --tui only) ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum TunnelPhase {
    Connecting,
    Up,
    Reconnecting { backoff: Duration },
    Fatal(String),
}

#[derive(Debug)]
pub struct TunnelState {
    pub destination: String,
    pub phase: TunnelPhase,
    pub reconnects: u32,
    pub log: VecDeque<String>,
}

impl TunnelState {
    pub fn new(destination: String) -> Self {
        Self {
            destination,
            phase: TunnelPhase::Connecting,
            reconnects: 0,
            log: VecDeque::new(),
        }
    }

    fn push_log(&mut self, line: String) {
        self.log.push_back(line);
        if self.log.len() > TUNNEL_LOG_CAP {
            self.log.pop_front();
        }
    }
}

// ── the app ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tab {
    Overview,
    History,
    Tunnel,
}

pub struct App {
    pub base_url: String,
    pub health: Option<Health>,
    pub latency: Option<Duration>,
    /// Last poll error (server unreachable, ...) — shown in the header.
    pub error: Option<String>,
    pub sessions: Vec<SessionSummary>,
    /// Selection is tracked by id so it survives list reordering/removal.
    pub selected_sid: Option<String>,
    pub detail: Option<Detail>,
    pub tab: Tab,
    /// History-pane scroll offset from the tail (0 = follow newest).
    pub history_scroll: usize,
    pub tunnel: Option<TunnelState>,
    /// Session id awaiting delete confirmation (modal open).
    pub confirm_delete: Option<String>,
    pub should_quit: bool,
}

/// Side effects `on_key` asks the caller (the event loop) to perform.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Refresh,
    DeleteSession(String),
}

impl App {
    pub fn new(base_url: String, tunnel: Option<TunnelState>) -> Self {
        Self {
            base_url,
            health: None,
            latency: None,
            error: None,
            sessions: Vec::new(),
            selected_sid: None,
            detail: None,
            tab: Tab::Overview,
            history_scroll: 0,
            tunnel,
            confirm_delete: None,
            should_quit: false,
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        let sid = self.selected_sid.as_deref()?;
        self.sessions.iter().position(|s| s.session_id == sid)
    }

    pub fn selected_session(&self) -> Option<&SessionSummary> {
        self.selected_index().map(|i| &self.sessions[i])
    }

    /// Tabs available right now (Tunnel only exists under `connect --tui`).
    pub fn tabs(&self) -> Vec<Tab> {
        if self.tunnel.is_some() {
            vec![Tab::Overview, Tab::History, Tab::Tunnel]
        } else {
            vec![Tab::Overview, Tab::History]
        }
    }

    pub fn apply(&mut self, snap: Snapshot) {
        if let Some(h) = snap.health {
            self.health = Some(h);
        }
        self.latency = snap.latency.or(self.latency);
        self.error = snap.error;
        self.sessions = snap.sessions;

        // Keep the selection pinned to its session; fall back to the first
        // entry when it vanished (deleted / TTL-evicted) or nothing was
        // selected yet.
        if self.selected_index().is_none() {
            self.selected_sid = self.sessions.first().map(|s| s.session_id.clone());
        }
        // Only adopt detail that matches the current selection (the poller
        // may race a just-moved selection).
        if let Some(d) = snap.detail {
            if self.selected_sid.as_deref() == Some(d.session_id.as_str()) {
                self.detail = Some(d);
            }
        }
        if self
            .detail
            .as_ref()
            .is_some_and(|d| self.selected_sid.as_deref() != Some(d.session_id.as_str()))
        {
            self.detail = None;
        }
    }

    pub fn apply_tunnel(&mut self, event: TunnelEvent) {
        let Some(t) = self.tunnel.as_mut() else { return };
        match event {
            TunnelEvent::Up { reestablished, .. } => {
                if reestablished {
                    t.reconnects += 1;
                    t.push_log("tunnel re-established".into());
                } else {
                    t.push_log("tunnel up".into());
                }
                t.phase = TunnelPhase::Up;
            }
            TunnelEvent::StartingRemote { bin, dest } => {
                t.push_log(format!("starting remote server ({bin} on {dest})"));
            }
            TunnelEvent::Retrying { backoff, reason } => {
                t.push_log(format!("{reason}; reconnecting in {backoff:?}"));
                t.phase = TunnelPhase::Reconnecting { backoff };
            }
            TunnelEvent::SshLine(line) => t.push_log(format!("ssh: {line}")),
            TunnelEvent::VersionMismatch { local, remote } => {
                t.push_log(format!(
                    "WARNING version mismatch: remote {remote} vs client {local}"
                ));
            }
            TunnelEvent::Fatal { reason } => {
                t.push_log(format!("FATAL: {reason}"));
                t.phase = TunnelPhase::Fatal(reason);
            }
        }
    }

    fn select_delta(&mut self, delta: isize) {
        if self.sessions.is_empty() {
            self.selected_sid = None;
            return;
        }
        let cur = self.selected_index().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.sessions.len() as isize - 1) as usize;
        let sid = self.sessions[next].session_id.clone();
        if self.selected_sid.as_deref() != Some(sid.as_str()) {
            self.selected_sid = Some(sid);
            self.detail = None;
            self.history_scroll = 0;
        }
    }

    fn cycle_tab(&mut self, forward: bool) {
        let tabs = self.tabs();
        let cur = tabs.iter().position(|t| *t == self.tab).unwrap_or(0);
        let next = if forward {
            (cur + 1) % tabs.len()
        } else {
            (cur + tabs.len() - 1) % tabs.len()
        };
        self.tab = tabs[next];
    }

    /// Interpret one key press. `code` is a normalized name: single chars as
    /// themselves plus "up"/"down"/"tab"/"backtab"/"esc"/"pageup"/"pagedown"/
    /// "end"/"ctrl-c". Returns the side effect for the event loop to run.
    pub fn on_key(&mut self, code: &str) -> Action {
        // Modal first: it swallows everything.
        if let Some(sid) = self.confirm_delete.clone() {
            return match code {
                "y" => {
                    self.confirm_delete = None;
                    Action::DeleteSession(sid)
                }
                "n" | "esc" | "q" => {
                    self.confirm_delete = None;
                    Action::None
                }
                _ => Action::None,
            };
        }

        match code {
            "q" | "ctrl-c" => {
                self.should_quit = true;
                Action::None
            }
            "j" | "down" => {
                self.select_delta(1);
                Action::Refresh
            }
            "k" | "up" => {
                self.select_delta(-1);
                Action::Refresh
            }
            "tab" | "l" => {
                self.cycle_tab(true);
                Action::None
            }
            "backtab" | "h" => {
                self.cycle_tab(false);
                Action::None
            }
            "r" => Action::Refresh,
            "d" => {
                if let Some(s) = self.selected_session() {
                    self.confirm_delete = Some(s.session_id.clone());
                }
                Action::None
            }
            "pageup" => {
                let len = self.detail.as_ref().map_or(0, |d| d.history.len());
                self.history_scroll = (self.history_scroll + 10).min(len.saturating_sub(1));
                Action::None
            }
            "pagedown" => {
                self.history_scroll = self.history_scroll.saturating_sub(10);
                Action::None
            }
            "end" => {
                self.history_scroll = 0;
                Action::None
            }
            _ => Action::None,
        }
    }
}

// ── shared formatting helpers (used by ui.rs) ─────────────────────────────────

pub fn fmt_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

pub fn fmt_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    match bytes {
        0..=1023 => format!("{bytes}B"),
        1024..=1_048_575 => format!("{}KiB", bytes / 1024),
        1_048_576..=1_073_741_823 => format!("{}MiB", bytes / MIB),
        _ => format!("{:.1}GiB", bytes as f64 / (MIB as f64 * 1024.0)),
    }
}

/// Shorten a UUID-ish session id for the left rail.
pub fn short_id(sid: &str) -> String {
    if sid.len() > 8 {
        format!("{}…", &sid[..8])
    } else {
        sid.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str) -> SessionSummary {
        SessionSummary {
            session_id: id.into(),
            created_unix: 1,
            idle_secs: 0,
            active_ref: None,
            image_count: 0,
            cache_bytes: 0,
            running_jobs: 0,
            last_seq: 0,
        }
    }

    fn app_with(ids: &[&str]) -> App {
        let mut app = App::new("http://127.0.0.1:1".into(), None);
        app.apply(Snapshot {
            sessions: ids.iter().map(|i| summary(i)).collect(),
            ..Default::default()
        });
        app
    }

    #[test]
    fn selection_defaults_clamps_and_follows_id() {
        let mut app = app_with(&["a", "b", "c"]);
        assert_eq!(app.selected_sid.as_deref(), Some("a"));

        app.on_key("j");
        app.on_key("j");
        app.on_key("j"); // clamped at the end
        assert_eq!(app.selected_sid.as_deref(), Some("c"));
        app.on_key("k");
        assert_eq!(app.selected_sid.as_deref(), Some("b"));

        // "b" survives a reorder; detail for it is kept.
        app.detail = Some(Detail { session_id: "b".into(), ..Default::default() });
        app.apply(Snapshot {
            sessions: vec![summary("c"), summary("b"), summary("a")],
            ..Default::default()
        });
        assert_eq!(app.selected_sid.as_deref(), Some("b"));
        assert!(app.detail.is_some());

        // "b" deleted → fall back to first, stale detail dropped.
        app.apply(Snapshot {
            sessions: vec![summary("c"), summary("a")],
            ..Default::default()
        });
        assert_eq!(app.selected_sid.as_deref(), Some("c"));
        assert!(app.detail.is_none());
    }

    #[test]
    fn mismatched_detail_from_racing_poll_is_ignored() {
        let mut app = app_with(&["a", "b"]);
        app.apply(Snapshot {
            sessions: vec![summary("a"), summary("b")],
            detail: Some(Detail { session_id: "b".into(), ..Default::default() }),
            ..Default::default()
        });
        assert!(app.detail.is_none(), "detail for unselected session adopted");
    }

    #[test]
    fn tab_cycling_respects_tunnel_presence() {
        let mut app = app_with(&["a"]);
        app.on_key("tab");
        assert_eq!(app.tab, Tab::History);
        app.on_key("tab");
        assert_eq!(app.tab, Tab::Overview); // no Tunnel tab without a tunnel

        app.tunnel = Some(TunnelState::new("olaf1".into()));
        app.on_key("backtab");
        assert_eq!(app.tab, Tab::Tunnel);
    }

    #[test]
    fn delete_flow_requires_confirmation() {
        let mut app = app_with(&["a", "b"]);
        assert_eq!(app.on_key("d"), Action::None);
        assert_eq!(app.confirm_delete.as_deref(), Some("a"));
        // Modal swallows navigation.
        assert_eq!(app.on_key("j"), Action::None);
        assert_eq!(app.selected_sid.as_deref(), Some("a"));
        // n cancels; y confirms.
        app.on_key("n");
        assert!(app.confirm_delete.is_none());
        app.on_key("d");
        assert_eq!(app.on_key("y"), Action::DeleteSession("a".into()));
        assert!(app.confirm_delete.is_none());
    }

    #[test]
    fn quit_and_refresh_keys() {
        let mut app = app_with(&["a"]);
        assert_eq!(app.on_key("r"), Action::Refresh);
        assert!(!app.should_quit);
        app.on_key("q");
        assert!(app.should_quit);
    }

    #[test]
    fn tunnel_events_update_phase_and_counters() {
        let mut app = App::new("http://x:1".into(), Some(TunnelState::new("olaf1".into())));
        app.apply_tunnel(TunnelEvent::Up { health: "{}".into(), reestablished: false });
        assert_eq!(app.tunnel.as_ref().unwrap().phase, TunnelPhase::Up);
        app.apply_tunnel(TunnelEvent::Retrying {
            backoff: Duration::from_secs(1),
            reason: "tunnel dropped (signal: 9)".into(),
        });
        assert!(matches!(
            app.tunnel.as_ref().unwrap().phase,
            TunnelPhase::Reconnecting { .. }
        ));
        app.apply_tunnel(TunnelEvent::Up { health: "{}".into(), reestablished: true });
        let t = app.tunnel.as_ref().unwrap();
        assert_eq!(t.phase, TunnelPhase::Up);
        assert_eq!(t.reconnects, 1);
        assert!(t.log.iter().any(|l| l.contains("re-established")));
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(fmt_age(45), "45s");
        assert_eq!(fmt_age(180), "3m");
        assert_eq!(fmt_age(7200), "2h");
        assert_eq!(fmt_age(200_000), "2d");
        assert_eq!(fmt_bytes(512), "512B");
        assert_eq!(fmt_bytes(10 * 1024 * 1024), "10MiB");
        assert_eq!(short_id("063c3f73-9105-47e3"), "063c3f73…");
    }
}
