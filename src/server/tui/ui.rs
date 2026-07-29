//! Rendering: App → ratatui widgets. Layout per issue #3:
//!
//! ```text
//! ┌ header: server · version · io mode · latency · tunnel badge ─┐
//! │ Sessions (left rail) │ [Overview] [History] [Tunnel]         │
//! │                      │ tab body                              │
//! ├ footer: key hints ───────────────────────────────────────────┤
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, Tabs, Wrap,
};
use ratatui::Frame;

use super::app::{
    fmt_age, fmt_bytes, short_id, App, Detail, HistoryEvent, Tab, TunnelPhase, TunnelState,
};

pub fn draw(frame: &mut Frame, app: &App) {
    let [header, main, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);

    let [rail, body] =
        Layout::horizontal([Constraint::Length(24), Constraint::Min(20)]).areas(main);
    draw_sessions(frame, app, rail);
    draw_body(frame, app, body);

    draw_footer(frame, app, footer);

    if let Some(sid) = &app.confirm_delete {
        draw_confirm(frame, sid);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans: Vec<Span> = vec![Span::styled(
        " astroburst-server ",
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if let Some(t) = &app.tunnel {
        spans.push(Span::raw(format!("@ {} (ssh) ", t.destination)));
    }
    spans.push(Span::raw(format!("· {} ", app.base_url)));
    if let Some(h) = &app.health {
        spans.push(Span::raw(format!("· v{} · io:{} ", h.version, h.io_mode)));
    }
    if let Some(lat) = app.latency {
        spans.push(Span::raw(format!("· {}ms ", lat.as_millis())));
    }
    if let Some(err) = &app.error {
        spans.push(Span::styled(
            format!("· {err} "),
            Style::new().fg(Color::Red),
        ));
    }
    if let Some(t) = &app.tunnel {
        spans.push(tunnel_badge(t));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).reversed(), area);
}

fn tunnel_badge(t: &TunnelState) -> Span<'static> {
    match &t.phase {
        TunnelPhase::Connecting => Span::styled("· tunnel: connecting ", Style::new().fg(Color::Yellow)),
        TunnelPhase::Up => Span::styled("· tunnel: up ", Style::new().fg(Color::Green)),
        TunnelPhase::Reconnecting { backoff } => Span::styled(
            format!("· tunnel: reconnecting ({backoff:?}) "),
            Style::new().fg(Color::Yellow),
        ),
        TunnelPhase::Fatal(_) => Span::styled("· tunnel: FAILED ", Style::new().fg(Color::Red)),
    }
}

fn draw_sessions(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            let mut spans = vec![Span::raw(format!("{:<9} ", short_id(&s.session_id)))];
            if s.running_jobs > 0 {
                spans.push(Span::styled(
                    format!("● {} job{}", s.running_jobs, if s.running_jobs == 1 { "" } else { "s" }),
                    Style::new().fg(Color::Yellow),
                ));
            } else {
                spans.push(Span::styled(
                    format!("idle {}", fmt_age(s.idle_secs)),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = format!(" Sessions ({}) ", app.sessions.len());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶");

    let mut state = ListState::default();
    state.select(app.selected_index());
    frame.render_stateful_widget(list, area, &mut state);
}

fn tab_title(tab: Tab) -> &'static str {
    match tab {
        Tab::Overview => "Overview",
        Tab::History => "History",
        Tab::Global => "Global",
        Tab::Tunnel => "Tunnel",
    }
}

fn draw_body(frame: &mut Frame, app: &App, area: Rect) {
    let [tabs_area, content] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(2)]).areas(area);

    let tabs = app.tabs();
    let selected = tabs.iter().position(|t| *t == app.tab).unwrap_or(0);
    frame.render_widget(
        Tabs::new(tabs.iter().map(|t| tab_title(*t)).collect::<Vec<_>>())
            .select(selected)
            .highlight_style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
        tabs_area,
    );

    match app.tab {
        Tab::Overview => draw_overview(frame, app, content),
        Tab::History => draw_history(frame, app, content),
        Tab::Global => draw_global_activity(frame, app, content),
        Tab::Tunnel => draw_tunnel(frame, app, content),
    }
}

fn draw_overview(frame: &mut Frame, app: &App, area: Rect) {
    let Some(s) = app.selected_session() else {
        frame.render_widget(
            Paragraph::new("no sessions — POST /v2/sessions to create one")
                .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let [info_area, images_area] =
        Layout::vertical([Constraint::Length(6), Constraint::Min(3)]).areas(area);

    let age = now_unix().saturating_sub(s.created_unix);
    let info = vec![
        Line::from(format!("session   {}", s.session_id)),
        Line::from(format!(
            "created   {} ago     idle {}",
            fmt_age(age),
            fmt_age(s.idle_secs)
        )),
        Line::from(format!(
            "images    {}     cache {}     running jobs {}",
            s.image_count,
            fmt_bytes(s.cache_bytes),
            s.running_jobs
        )),
        Line::from(format!(
            "active    {}",
            s.active_ref.as_deref().unwrap_or("—")
        )),
    ];
    frame.render_widget(
        Paragraph::new(info).block(Block::default().borders(Borders::ALL).title(" Status ")),
        info_area,
    );

    let images = app
        .detail
        .as_ref()
        .filter(|d| d.session_id == s.session_id)
        .map(|d| d.images.as_slice())
        .unwrap_or(&[]);
    let rows: Vec<Row> = images
        .iter()
        .map(|img| {
            Row::new(vec![
                img.image_ref.clone(),
                format!("{}×{}", img.width, img.height),
                img.hdu.map(|h| h.to_string()).unwrap_or_else(|| "—".into()),
                img.extname.clone().unwrap_or_else(|| "—".into()),
                img.source
                    .as_deref()
                    .map(basename)
                    .unwrap_or_else(|| "(derived)".into()),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(11),
            Constraint::Length(4),
            Constraint::Length(10),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new(vec!["ref", "dims", "hdu", "extname", "source"])
            .style(Style::new().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(" Images "));
    frame.render_widget(table, images_area);
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn draw_history(frame: &mut Frame, app: &App, area: Rect) {
    let empty = Detail::default();
    let detail = app.detail.as_ref().unwrap_or(&empty);
    draw_event_table(frame, area, &detail.history, app.history_scroll, "History", "ref");
}

fn draw_global_activity(frame: &mut Frame, app: &App, area: Rect) {
    // Sessionless /v2/fs/* calls; the `ref` column carries the request query.
    draw_event_table(
        frame,
        area,
        &app.global_activity,
        app.global_scroll,
        "Global activity",
        "query",
    );
}

/// Shared renderer for an activity event table (per-session History and the
/// sessionless Global feed). Tail-follows the newest rows, shifted back by
/// `scroll`; `ref_header` labels the `image_ref`/query column.
fn draw_event_table(
    frame: &mut Frame,
    area: Rect,
    events: &[HistoryEvent],
    scroll: usize,
    title_label: &str,
    ref_header: &str,
) {
    let now_ms = now_unix() * 1000;

    let visible = area.height.saturating_sub(3) as usize; // borders + header row
    let total = events.len();
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(visible);

    let rows: Vec<Row> = events[start..end]
        .iter()
        .map(|e| {
            let status_style = if e.status < 400 {
                Style::new().fg(Color::Green)
            } else {
                Style::new().fg(Color::Red)
            };
            Row::new(vec![
                Span::raw(format!("{:>5}", e.seq)),
                Span::styled(
                    format!("{:>4} ago", fmt_age(now_ms.saturating_sub(e.unix_ms) / 1000)),
                    Style::new().fg(Color::DarkGray),
                ),
                Span::raw(format!("{:<6}", e.method)),
                Span::raw(e.endpoint.clone()),
                Span::raw(e.image_ref.clone().unwrap_or_else(|| "—".into())),
                Span::styled(e.status.to_string(), status_style),
                Span::raw(format!("{}ms", e.duration_ms)),
            ])
        })
        .collect();

    let title = if scroll > 0 {
        format!(" {title_label} ({total} events, ↑{scroll}) ")
    } else {
        format!(" {title_label} ({total} events) ")
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(20),
            Constraint::Length(12),
            Constraint::Length(4),
            Constraint::Min(6),
        ],
    )
    .header(
        Row::new(vec!["seq", "when", "meth", "endpoint", ref_header, "st", "took"])
            .style(Style::new().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(table, area);
}

fn draw_tunnel(frame: &mut Frame, app: &App, area: Rect) {
    let Some(t) = &app.tunnel else {
        frame.render_widget(
            Paragraph::new("no tunnel (direct connection)")
                .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let [status_area, log_area] =
        Layout::vertical([Constraint::Length(5), Constraint::Min(3)]).areas(area);

    let phase_line = match &t.phase {
        TunnelPhase::Connecting => Line::from(Span::styled("connecting…", Style::new().fg(Color::Yellow))),
        TunnelPhase::Up => Line::from(Span::styled("up", Style::new().fg(Color::Green))),
        TunnelPhase::Reconnecting { backoff } => Line::from(Span::styled(
            format!("reconnecting (next attempt in {backoff:?})"),
            Style::new().fg(Color::Yellow),
        )),
        TunnelPhase::Fatal(reason) => Line::from(Span::styled(
            format!("FAILED: {reason}"),
            Style::new().fg(Color::Red),
        )),
    };
    let status = vec![
        Line::from(format!("destination  {}", t.destination)),
        {
            let mut l = phase_line;
            l.spans.insert(0, Span::raw("state        "));
            l
        },
        Line::from(format!("reconnects   {}", t.reconnects)),
    ];
    frame.render_widget(
        Paragraph::new(status).block(Block::default().borders(Borders::ALL).title(" Tunnel ")),
        status_area,
    );

    let visible = log_area.height.saturating_sub(2) as usize;
    let start = t.log.len().saturating_sub(visible);
    let lines: Vec<Line> = t.log.iter().skip(start).map(|l| Line::from(l.as_str())).collect();
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(" Log ")),
        log_area,
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let hints = if app.confirm_delete.is_some() {
        " y confirm delete · n cancel".to_string()
    } else {
        let tunnel = if app.tunnel.is_some() { " · tab panes incl. Tunnel" } else { "" };
        format!(" j/k select · tab pane · d delete · r refresh · PgUp/PgDn scroll · q quit{tunnel}")
    };
    frame.render_widget(
        Paragraph::new(hints).style(Style::new().fg(Color::DarkGray)),
        area,
    );
}

fn draw_confirm(frame: &mut Frame, sid: &str) {
    let area = frame.area();
    let w = (area.width.saturating_sub(4)).min(60);
    let h = 5;
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h.min(area.height),
    };
    frame.render_widget(Clear, rect);
    let text = vec![
        Line::from(""),
        Line::from(format!("Delete session {}?", short_id(sid))).alignment(Alignment::Center),
        Line::from(Span::styled("y = delete    n = cancel", Style::new().fg(Color::DarkGray)))
            .alignment(Alignment::Center),
    ];
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Red))
                .title(" confirm "),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{HistoryEvent, ImageEntry, SessionSummary, Snapshot};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn populated_app() -> App {
        let mut app = App::new("http://127.0.0.1:8097".into(), None);
        app.apply(Snapshot {
            health: Some(super::super::app::Health {
                version: "0.2.0".into(),
                io_mode: "auto".into(),
            }),
            latency: Some(std::time::Duration::from_millis(3)),
            sessions: vec![SessionSummary {
                session_id: "063c3f73-9105-47e3".into(),
                created_unix: 1,
                idle_secs: 42,
                active_ref: Some("img_0".into()),
                image_count: 1,
                cache_bytes: 10 * 1024 * 1024,
                running_jobs: 0,
                last_seq: 2,
            }],
            detail: Some(Detail {
                session_id: "063c3f73-9105-47e3".into(),
                images: vec![ImageEntry {
                    image_ref: "img_0".into(),
                    width: 1600,
                    height: 1600,
                    hdu: Some(0),
                    extname: None,
                    source: Some("/data/656nmos.fits".into()),
                }],
                history: vec![HistoryEvent {
                    seq: 1,
                    unix_ms: 1,
                    method: "POST".into(),
                    endpoint: "open".into(),
                    image_ref: Some("img_0".into()),
                    status: 200,
                    duration_ms: 249,
                }],
            }),
            global_activity: vec![HistoryEvent {
                seq: 1,
                unix_ms: 1,
                method: "GET".into(),
                endpoint: "/v2/fs/raw".into(),
                image_ref: Some("path=/data/656nmos.fits&compress=lossless".into()),
                status: 200,
                duration_ms: 612,
            }],
            error: None,
        });
        app
    }

    #[test]
    fn overview_renders_session_and_images() {
        let app = populated_app();
        let screen = render(&app);
        assert!(screen.contains("063c3f73…"), "left rail id missing:\n{screen}");
        assert!(screen.contains("Sessions (1)"));
        assert!(screen.contains("v0.2.0"));
        assert!(screen.contains("io:auto"));
        assert!(screen.contains("img_0"));
        assert!(screen.contains("1600×1600"));
        assert!(screen.contains("656nmos.fits"));
        assert!(screen.contains("idle 42s"));
    }

    #[test]
    fn history_tab_renders_events() {
        let mut app = populated_app();
        app.tab = Tab::History;
        let screen = render(&app);
        assert!(screen.contains("open"), "{screen}");
        assert!(screen.contains("POST"));
        assert!(screen.contains("249ms"));
        assert!(screen.contains("History (1 events)"));
    }

    #[test]
    fn global_tab_renders_fs_activity() {
        let mut app = populated_app();
        app.tab = Tab::Global;
        let screen = render(&app);
        assert!(screen.contains("Global activity (1 events)"), "{screen}");
        assert!(screen.contains("/v2/fs/raw"), "{screen}");
        assert!(screen.contains("612ms"));
        assert!(screen.contains("query"), "query column header missing:\n{screen}");
    }

    #[test]
    fn tunnel_tab_and_badge_render() {
        let mut app = populated_app();
        app.tunnel = Some(TunnelState::new("olaf1".into()));
        app.apply_tunnel(crate::connect::TunnelEvent::Up {
            health: "{}".into(),
            reestablished: false,
        });
        app.tab = Tab::Tunnel;
        let screen = render(&app);
        assert!(screen.contains("tunnel: up"), "{screen}");
        assert!(screen.contains("destination  olaf1"));
        assert!(screen.contains("reconnects   0"));
    }

    #[test]
    fn confirm_modal_renders_over_content() {
        let mut app = populated_app();
        app.on_key("d");
        let screen = render(&app);
        assert!(screen.contains("Delete session 063c3f73…?"), "{screen}");
        assert!(screen.contains("y = delete"));
    }

    #[test]
    fn empty_server_renders_hint() {
        let app = App::new("http://127.0.0.1:8097".into(), None);
        let screen = render(&app);
        assert!(screen.contains("no sessions"), "{screen}");
    }
}
