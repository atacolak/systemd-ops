//! Read-only operational console for one responsibility scope.
//!
//! Consumes [`crate::scope::show`]. Does not shell the CLI. Does not
//! mutate systemd.

use std::io::{self, stdout};
use std::process::Command;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use ratatui::Terminal;
use serde_json::Value;

use crate::scope::{self, ScopeView};
use crate::systemd::{self, BackendError, LogFilter};

const REFRESH_EVERY: Duration = Duration::from_secs(3);
const TALL_LOGS: u16 = 24;

const BORDER: Color = Color::Rgb(86, 90, 108);
const MUTED: Color = Color::Rgb(138, 142, 158);
const TEXT: Color = Color::Rgb(220, 222, 228);
const ACCENT: Color = Color::Rgb(137, 196, 220);
const SEL_BG: Color = Color::Rgb(46, 50, 66);
const GREEN: Color = Color::Rgb(166, 209, 137);
const RED: Color = Color::Rgb(232, 136, 136);
const YELLOW: Color = Color::Rgb(230, 197, 120);

struct App {
    view: ScopeView,
    rows: Vec<ListRow>,
    list: ListState,
    filter: String,
    filtering: bool,
    logs: Vec<LogLine>,
    last_error: Option<String>,
    last_refresh: Instant,
    detail_expanded: bool,
    logs_force: Option<bool>,
    last_frame: Rect,
}

#[derive(Clone)]
enum ListRow {
    Header(&'static str),
    Op(Box<Row>),
}

#[derive(Clone)]
struct LogLine {
    text: String,
    alert: bool,
}

#[derive(Clone)]
struct Row {
    unit: String,
    title: String,
    health: String,
    relationship: &'static str,
    critical: bool,
    next: String,
    kind: String,
    management: String,
    purpose: String,
    tags: String,
    origin: String,
    origin_scope: String,
    state: String,
    sub: String,
    last_result: String,
    last: String,
    exec: String,
    cwd: String,
    fragments: String,
    health_basis: String,
    activation: String,
}

pub(crate) fn logs_unit(stem: &str) -> String {
    format!("{stem}.service")
}

fn sectioned_rows(owned: Vec<Row>, watching: Vec<Row>) -> Vec<ListRow> {
    let mut rows = Vec::new();
    rows.push(ListRow::Header("OWNED"));
    if owned.is_empty() {
        rows.push(ListRow::Header("all quiet"));
    } else {
        rows.extend(owned.into_iter().map(|r| ListRow::Op(Box::new(r))));
    }
    if !watching.is_empty() {
        rows.push(ListRow::Header("WATCHING"));
        rows.extend(watching.into_iter().map(|r| ListRow::Op(Box::new(r))));
    }
    rows
}

fn health_rank(health: &str) -> u8 {
    match health {
        "failed" => 0,
        "unknown" => 1,
        _ => 2,
    }
}

impl App {
    fn load(cwd: Option<&str>) -> Result<Self, BackendError> {
        let view = scope::show(cwd)?;
        let mut app = Self {
            rows: Vec::new(),
            view,
            list: ListState::default(),
            filter: String::new(),
            filtering: false,
            logs: Vec::new(),
            last_error: None,
            last_refresh: Instant::now(),
            detail_expanded: false,
            logs_force: None,
            last_frame: Rect::default(),
        };
        app.rebuild_rows();
        app.reload_logs();
        Ok(app)
    }

    fn rebuild_rows(&mut self) {
        let mut owned = Vec::new();
        for v in &self.view.owned {
            if let Some(r) = row_from_view(v, "owned") {
                if self.filter_matches(&r) {
                    owned.push(r);
                }
            }
        }
        owned.sort_by_key(|r| health_rank(&r.health));
        let mut watching = Vec::new();
        for v in &self.view.watching {
            if let Some(r) = row_from_view(v, "watching") {
                if self.filter_matches(&r) {
                    watching.push(r);
                }
            }
        }
        watching.sort_by_key(|r| health_rank(&r.health));
        let keep = self.selected().map(|r| r.unit.clone());
        self.rows = sectioned_rows(owned, watching);
        if let Some(unit) = keep {
            if let Some(i) = self.rows.iter().position(|r| match r {
                ListRow::Op(op) => op.unit == unit,
                ListRow::Header(_) => false,
            }) {
                self.list.select(Some(i));
            } else {
                self.select_first_op();
            }
        } else {
            self.select_first_op();
        }
    }

    fn select_first_op(&mut self) {
        let i = self.rows.iter().position(|r| matches!(r, ListRow::Op(_)));
        self.list.select(i);
    }

    fn filter_matches(&self, row: &Row) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_ascii_lowercase();
        row.unit.to_ascii_lowercase().contains(&q)
            || row.title.to_ascii_lowercase().contains(&q)
            || row.purpose.to_ascii_lowercase().contains(&q)
            || row.tags.to_ascii_lowercase().contains(&q)
    }

    fn selected(&self) -> Option<&Row> {
        self.list.selected().and_then(|i| match self.rows.get(i) {
            Some(ListRow::Op(r)) => Some(r.as_ref()),
            _ => None,
        })
    }

    fn move_sel(&mut self, dir: i32) {
        let cur = self.list.selected().unwrap_or(0) as i32;
        let len = self.rows.len() as i32;
        if len == 0 {
            return;
        }
        let mut i = cur;
        for _ in 0..len {
            i = (i + dir).rem_euclid(len);
            if matches!(self.rows.get(i as usize), Some(ListRow::Op(_))) {
                self.list.select(Some(i as usize));
                self.reload_logs();
                return;
            }
        }
    }

    fn reload_logs(&mut self) {
        let Some(row) = self.selected() else {
            self.logs.clear();
            return;
        };
        let unit = logs_unit(&row.unit);
        let filter = LogFilter {
            lines: 40,
            priority: None,
            since: None,
            until: None,
            boot: None,
            grep: None,
        };
        match systemd::unit_logs(&unit, &filter) {
            Ok(payload) => {
                self.logs = extract_log_entries(&payload)
                    .iter()
                    .map(format_log)
                    .collect();
                self.last_error = None;
            }
            Err(e) => {
                self.logs = vec![LogLine {
                    text: format!("logs unavailable: {e}"),
                    alert: true,
                }];
            }
        }
    }

    fn refresh(&mut self, cwd: Option<&str>) {
        match scope::show(cwd) {
            Ok(view) => {
                self.view = view;
                self.last_error = None;
                self.rebuild_rows();
                self.reload_logs();
                self.last_refresh = Instant::now();
            }
            Err(e) => self.last_error = Some(e.0),
        }
    }

    fn logs_shown(&self) -> bool {
        self.logs_force
            .unwrap_or(self.last_frame.height >= TALL_LOGS)
    }

    fn detail_shown(&self) -> bool {
        true
    }

    fn toggle_logs(&mut self) {
        let showing = self.logs_shown();
        self.logs_force = Some(!showing);
        if !showing {
            self.reload_logs();
        }
    }
}

fn row_from_view(v: &Value, relationship: &'static str) -> Option<Row> {
    let unit = v.get("unit")?.as_str()?.to_string();
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(&unit)
        .to_string();
    let next = match v.get("next") {
        Some(Value::Null) | None => String::new(),
        Some(x) => x.as_str().unwrap_or(&x.to_string()).to_string(),
    };
    let tags = v
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let exec = v
        .get("exec")
        .map(|e| {
            let path = e.get("path").and_then(Value::as_str).unwrap_or("");
            let argv = e
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            format!("{path} {argv}").trim().to_string()
        })
        .unwrap_or_default();
    let fragments = v
        .get("fragment_paths")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    Some(Row {
        unit,
        title,
        health: json_str(v, "health"),
        relationship,
        critical: v.get("critical").and_then(Value::as_bool).unwrap_or(false),
        next,
        kind: json_str(v, "kind"),
        management: json_str(v, "management"),
        purpose: json_str(v, "purpose"),
        tags,
        origin: json_str(v, "origin_cwd"),
        origin_scope: json_str(v, "origin_scope"),
        state: json_str(v, "state"),
        sub: json_str(v, "sub"),
        last_result: json_str(v, "last_result"),
        last: json_str(v, "last"),
        exec,
        cwd: json_str(v, "cwd"),
        fragments,
        health_basis: json_str(v, "health_basis"),
        activation: json_str(v, "activation"),
    })
}

fn json_str(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::Null) | None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn extract_log_entries(payload: &Value) -> Vec<Value> {
    match payload {
        Value::Array(entries) => entries.clone(),
        Value::Object(map) => map
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn format_log(entry: &Value) -> LogLine {
    let ts = entry
        .get("timestamp")
        .or_else(|| entry.get("realtime"))
        .or_else(|| entry.get("__REALTIME_TIMESTAMP"))
        .map(|v| v.as_str().unwrap_or(&v.to_string()).to_string())
        .unwrap_or_default();
    let msg = entry
        .get("message")
        .or_else(|| entry.get("MESSAGE"))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| entry.to_string());
    let msg = collapse_ws(&msg);
    let clock = parse_rfc3339_utc(&ts)
        .map(|secs| format_clock_at(secs, local_offset_secs()))
        .unwrap_or_else(|| ts.chars().take(8).collect());
    let text = if clock.is_empty() {
        msg
    } else {
        format!("{clock}  {msg}")
    };
    let alert = log_is_alert(&text);
    LogLine { text, alert }
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in s.chars() {
        if c.is_whitespace() {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    out
}

fn log_is_alert(msg: &str) -> bool {
    let l = msg.to_ascii_lowercase();
    l.contains("traceback")
        || l.contains("failed with result")
        || l.contains("typeerror")
        || l.contains("exception")
        || l.contains("error:")
        || l.contains("status=1")
}

fn health_style(health: &str) -> Style {
    match health {
        "healthy" => Style::default().fg(GREEN),
        "failed" => Style::default().fg(RED),
        "degraded" => Style::default().fg(YELLOW),
        _ => Style::default().fg(MUTED),
    }
}

fn mark(health: &str) -> &'static str {
    match health {
        "healthy" => "●",
        "failed" => "✖",
        _ => "?",
    }
}

fn panel(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            title.into(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

pub fn run(cwd: Option<&str>) -> Result<(), BackendError> {
    let mut app = App::load(cwd)?;
    enable_raw_mode().map_err(|e| BackendError(format!("tui: {e}")))?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| BackendError(format!("tui: {e}")))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| BackendError(format!("tui: {e}")))?;
    let result = event_loop(&mut terminal, &mut app, cwd);
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    let _ = execute!(out, LeaveAlternateScreen);
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    cwd: Option<&str>,
) -> Result<(), BackendError> {
    loop {
        terminal
            .draw(|f| draw(f, app))
            .map_err(|e| BackendError(format!("tui: {e}")))?;
        if !event::poll(Duration::from_millis(250))
            .map_err(|e| BackendError(format!("tui: {e}")))?
        {
            if app.last_refresh.elapsed() >= REFRESH_EVERY {
                app.refresh(cwd);
            }
            continue;
        }
        let Event::Key(key) = event::read().map_err(|e| BackendError(format!("tui: {e}")))? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if app.filtering {
            match key.code {
                KeyCode::Esc => {
                    app.filtering = false;
                    app.filter.clear();
                    app.rebuild_rows();
                    app.reload_logs();
                }
                KeyCode::Enter => {
                    app.filtering = false;
                    app.rebuild_rows();
                    app.reload_logs();
                }
                KeyCode::Backspace => {
                    app.filter.pop();
                    app.rebuild_rows();
                }
                KeyCode::Char(c) => {
                    app.filter.push(c);
                    app.rebuild_rows();
                }
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('r') => app.refresh(cwd),
            KeyCode::Char('/') => app.filtering = true,
            KeyCode::Down | KeyCode::Char('j') => app.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_sel(-1),
            KeyCode::Char('l') => app.toggle_logs(),
            KeyCode::Char('d') => {
                app.detail_expanded = !app.detail_expanded;
            }
            KeyCode::Enter => {
                app.reload_logs();
                app.logs_force = Some(true);
            }
            _ => {}
        }
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    app.last_frame = f.area();
    let show_logs = app.logs_shown();
    let show_detail = app.detail_shown();
    let header_h = 4;
    let status_h = 1;
    let total = f.area().height;
    let log_h = if show_logs {
        (total / 4).clamp(5, 12)
    } else {
        0
    };
    let body_h = total
        .saturating_sub(header_h)
        .saturating_sub(status_h)
        .saturating_sub(log_h);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_h),
            Constraint::Length(body_h),
            Constraint::Length(log_h),
            Constraint::Length(status_h),
        ])
        .split(f.area());
    draw_header(f, chunks[0], app);
    if show_detail {
        let list_h = (app.rows.len() as u16)
            .saturating_add(2)
            .max(5)
            .min(body_h.saturating_sub(8));
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(list_h), Constraint::Min(8)])
            .split(chunks[1]);
        draw_list(f, body[0], app);
        draw_detail(f, body[1], app);
    } else {
        draw_list(f, chunks[1], app);
    }
    if log_h > 0 {
        draw_logs(f, chunks[2], app);
    }
    draw_status(f, chunks[3], app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let id = app.view.id.to_uppercase();
    let health = app.view.health.as_str().to_uppercase();
    let n_ops = app.view.owned.len() + app.view.watching.len();
    let n_fail = app
        .view
        .owned
        .iter()
        .chain(app.view.watching.iter())
        .filter(|v| v.get("health").and_then(Value::as_str) == Some("failed"))
        .count();
    let root = short_path(&app.view.root.display().to_string());
    let fail = if n_fail == 0 {
        Span::styled(format!("{n_ops} ops"), Style::default().fg(MUTED))
    } else {
        Span::styled(
            format!("{n_ops} ops  ·  {n_fail} failing"),
            Style::default().fg(RED),
        )
    };
    let text = vec![
        Line::from(vec![
            Span::styled(id, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
            Span::styled("   ", Style::default()),
            Span::styled(
                health,
                health_style(app.view.health.as_str()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   ", Style::default()),
            fail,
        ]),
        Line::from(Span::styled(root, Style::default().fg(MUTED))),
    ];
    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(BORDER)),
        ),
        area,
    );
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let inner = area.width.saturating_sub(2);
    let now = SystemTime::now();
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|r| match r {
            ListRow::Header(h) => {
                let quiet = *h == "all quiet";
                let style = if quiet {
                    Style::default().fg(MUTED)
                } else {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                };
                ListItem::new(Line::from(Span::styled(*h, style)))
            }
            ListRow::Op(r) => {
                let when = when_label(r, now);
                let line = op_line(&r.title, &r.health, &when, inner);
                ListItem::new(line)
            }
        })
        .collect();
    let title = if app.filtering {
        format!("ops  /{}", app.filter)
    } else {
        "ops".into()
    };
    let list = List::new(items).block(panel(title)).highlight_style(
        Style::default()
            .bg(SEL_BG)
            .fg(TEXT)
            .add_modifier(Modifier::BOLD),
    );
    let mut state = app.list.clone();
    f.render_stateful_widget(list, area, &mut state);
}

fn op_line(title: &str, health: &str, when: &str, inner: u16) -> Line<'static> {
    let mark = mark(health);
    let mark_w = 2usize;
    let when_w = when.chars().count();
    let budget = inner.saturating_sub(mark_w as u16 + 1) as usize;
    let title_budget = budget.saturating_sub(when_w);
    let title = truncate(title, title_budget);
    let used = mark_w + title.chars().count() + when_w;
    let pad = (inner as usize).saturating_sub(used);
    let mut spans = vec![
        Span::styled(format!("{mark} "), health_style(health)),
        Span::styled(title, Style::default().fg(TEXT)),
        Span::raw(" ".repeat(pad)),
    ];
    if !when.is_empty() {
        let when_style = if health == "failed" {
            Style::default().fg(RED)
        } else {
            Style::default().fg(MUTED)
        };
        spans.push(Span::styled(when.to_string(), when_style));
    }
    Line::from(spans)
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let now = SystemTime::now();
    let body = if let Some(r) = app.selected() {
        let mut lines = vec![
            Line::from(Span::styled(
                r.title.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                empty(&r.purpose).to_string(),
                Style::default().fg(MUTED),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "NEXT  ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    next_display(r, now),
                    Style::default()
                        .fg(Color::Rgb(250, 248, 240))
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("last  ", Style::default().fg(MUTED)),
                Span::styled(when_last(r, now), Style::default().fg(TEXT)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("exec  ", Style::default().fg(MUTED)),
                Span::styled(short_exec(&r.exec), Style::default().fg(TEXT)),
            ]),
            Line::from(vec![
                Span::styled("state ", Style::default().fg(MUTED)),
                Span::styled(state_line(r), Style::default().fg(TEXT)),
            ]),
        ];
        if app.detail_expanded {
            lines.push(Line::from(""));
            if !r.tags.is_empty() {
                lines.push(Line::from(vec![
                    muted_k("tags"),
                    Span::styled(r.tags.clone(), Style::default().fg(MUTED)),
                ]));
            }
            lines.push(Line::from(vec![
                muted_k("unit"),
                Span::styled(r.unit.clone(), Style::default().fg(MUTED)),
            ]));
            if !r.management.is_empty() {
                lines.push(Line::from(vec![
                    muted_k("mgmt"),
                    Span::styled(r.management.clone(), Style::default().fg(MUTED)),
                ]));
            }
            if r.relationship == "watching" {
                lines.push(Line::from(vec![
                    muted_k("role"),
                    Span::styled("watching", Style::default().fg(MUTED)),
                ]));
            }
            if !r.cwd.is_empty() {
                lines.push(Line::from(vec![
                    muted_k("cwd"),
                    Span::styled(short_path(&r.cwd), Style::default().fg(MUTED)),
                ]));
            }
            if !r.origin.is_empty() {
                lines.push(Line::from(vec![
                    muted_k("origin"),
                    Span::styled(short_path(&r.origin), Style::default().fg(MUTED)),
                ]));
            }
            if !r.origin_scope.is_empty() {
                lines.push(Line::from(vec![
                    muted_k("scope"),
                    Span::styled(r.origin_scope.clone(), Style::default().fg(MUTED)),
                ]));
            }
            for frag in r.fragments.lines().filter(|s| !s.is_empty()) {
                lines.push(Line::from(vec![
                    muted_k("file"),
                    Span::styled(short_path(frag), Style::default().fg(MUTED)),
                ]));
            }
        }
        lines
    } else {
        vec![Line::from(Span::styled(
            "nothing selected",
            Style::default().fg(MUTED),
        ))]
    };
    let title = app
        .selected()
        .map(|r| r.title.clone())
        .unwrap_or_else(|| "detail".into());
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: true })
            .block(panel(title)),
        area,
    );
}

fn muted_k(label: &str) -> Span<'static> {
    Span::styled(format!("{label:<6}"), Style::default().fg(MUTED))
}

fn draw_logs(f: &mut Frame, area: Rect, app: &App) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if app.logs.is_empty() {
        vec![Line::from(Span::styled(
            "quiet",
            Style::default().fg(MUTED),
        ))]
    } else {
        app.logs
            .iter()
            .map(|l| {
                let style = if l.alert {
                    Style::default().fg(RED)
                } else {
                    Style::default().fg(TEXT)
                };
                Line::from(Span::styled(l.text.clone(), style))
            })
            .collect()
    };
    let extra = lines.len().saturating_sub(inner_h.max(1));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .scroll((extra as u16, 0))
            .block(panel("logs")),
        area,
    );
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let msg = if let Some(err) = &app.last_error {
        Line::from(Span::styled(err.clone(), Style::default().fg(RED)))
    } else if app.filtering {
        Line::from(Span::styled(
            format!("filter  {}   enter apply · esc clear", app.filter),
            Style::default().fg(ACCENT),
        ))
    } else {
        Line::from(Span::styled(
            "q quit   j/k move   / find   r refresh   d details   l logs",
            Style::default().fg(MUTED),
        ))
    };
    f.render_widget(Paragraph::new(msg), area);
}

fn empty(s: &str) -> &str {
    if s.is_empty() {
        "—"
    } else {
        s
    }
}

fn short_path(p: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = p.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    p.to_string()
}

fn short_exec(exec: &str) -> String {
    if exec.is_empty() {
        return "—".into();
    }
    let tokens: Vec<String> = exec
        .split_whitespace()
        .map(|t| {
            if t.starts_with('/') {
                t.rsplit('/').next().unwrap_or(t).to_string()
            } else {
                t.to_string()
            }
        })
        .collect();
    if tokens.first().map(String::as_str) == Some("bash")
        && tokens.get(1).map(String::as_str) == Some("-c")
    {
        return tokens.into_iter().skip(2).collect::<Vec<_>>().join(" ");
    }
    tokens.join(" ")
}

fn state_line(r: &Row) -> String {
    let mut bits = Vec::new();
    if !r.sub.is_empty() {
        bits.push(r.sub.as_str());
    } else if !r.state.is_empty() {
        bits.push(r.state.as_str());
    }
    if !r.kind.is_empty() {
        bits.push(r.kind.as_str());
    }
    if !r.activation.is_empty() && r.activation != r.kind {
        bits.push(r.activation.as_str());
    }
    if r.critical {
        bits.push("critical");
    }
    if bits.is_empty() {
        r.health.clone()
    } else {
        bits.join(" · ")
    }
}

fn result_word(s: &str) -> &str {
    match s {
        "success" | "0" => "ok",
        "exit-code" => "exit",
        "" => "—",
        other => other,
    }
}

fn when_label(r: &Row, now: SystemTime) -> String {
    if r.health == "failed" {
        if !r.last.is_empty() {
            return relative_label(&r.last, now);
        }
        return "failed".into();
    }
    if r.sub == "running" || r.sub == "start" {
        return "running".into();
    }
    if (r.activation == "direct" || r.kind == "simple")
        && !r.sub.is_empty()
        && r.sub != "dead"
        && r.sub != "exited"
    {
        return r.sub.clone();
    }
    if !r.next.is_empty() {
        return countdown_label(&r.next, now);
    }
    if r.health_basis == "never-run" {
        return "never ran".into();
    }
    String::new()
}

fn countdown_label(stamp: &str, now: SystemTime) -> String {
    if stamp.is_empty() {
        return "—".into();
    }
    let Some(then) = parse_rfc3339_utc(stamp) else {
        return stamp.chars().take(16).collect();
    };
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(then);
    countdown_to(now_secs, then)
}

fn next_display(r: &Row, now: SystemTime) -> String {
    if r.sub == "running" || r.sub == "start" {
        return "running".into();
    }
    countdown_label(&r.next, now)
}

fn when_last(r: &Row, now: SystemTime) -> String {
    if r.last.is_empty() {
        result_word(&r.last_result).to_string()
    } else {
        let rel = relative_label(&r.last, now);
        let res = result_word(&r.last_result);
        if res == "—" {
            rel
        } else {
            format!("{rel}  {res}")
        }
    }
}

fn relative_label(stamp: &str, now: SystemTime) -> String {
    let Some(then) = parse_rfc3339_utc(stamp) else {
        return stamp.chars().take(16).collect();
    };
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(then);
    relative_to(now_secs, then)
}

fn countdown_to(now: i64, then: i64) -> String {
    let delta = then - now;
    if delta <= 0 {
        return "now".into();
    }
    let h = delta / 3_600;
    let m = (delta % 3_600) / 60;
    let s = delta % 60;
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

fn relative_to(now: i64, then: i64) -> String {
    let delta = then - now;
    let past = delta < 0;
    let abs = delta.abs();
    let body = if abs < 45 {
        return "now".into();
    } else if abs < 90 {
        "1m".into()
    } else if abs < 3_600 {
        format!("{}m", abs / 60)
    } else if abs < 90 * 60 {
        "1h".into()
    } else if abs < 86_400 {
        format!("{}h", abs / 3_600)
    } else if abs < 36 * 3600 {
        "1d".into()
    } else {
        format!("{}d", abs / 86_400)
    };
    if past {
        format!("{body} ago")
    } else {
        format!("in {body}")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 1 {
        return s.chars().take(max).collect();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn parse_rfc3339_utc(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (date, rest) = s.split_once('T')?;
    let rest = rest.strip_suffix('Z').unwrap_or(rest);
    let rest = rest.split(['+', '-']).next().unwrap_or(rest);
    let time = rest.split('.').next().unwrap_or(rest);
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next()?.parse().ok()?;
    Some(days_from_civil(y, m, day) * 86_400 + hh * 3_600 + mm * 60 + ss)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let mut y = y;
    let m = m as i64;
    let d = d as i64;
    if m <= 2 {
        y -= 1;
    }
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

static LOCAL_OFFSET_SECS: LazyLock<i64> = LazyLock::new(|| {
    Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| parse_tz_offset(std::str::from_utf8(&o.stdout).ok()?))
        .unwrap_or(0)
});

fn local_offset_secs() -> i64 {
    *LOCAL_OFFSET_SECS
}

fn parse_tz_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let digits = s.trim_start_matches(['+', '-']);
    if digits.len() < 4 {
        return None;
    }
    let h: i64 = digits[..2].parse().ok()?;
    let m: i64 = digits[2..4].parse().ok()?;
    Some(sign * (h * 3_600 + m * 60))
}

fn format_clock_at(utc_secs: i64, offset: i64) -> String {
    let local = utc_secs + offset;
    let sod = local.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        sod / 3_600,
        (sod % 3_600) / 60,
        sod % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systemd::usec_to_rfc3339;

    fn dummy(unit: &str, relationship: &'static str) -> Row {
        Row {
            unit: unit.into(),
            title: unit.into(),
            health: "unknown".into(),
            relationship,
            critical: false,
            next: String::new(),
            kind: "oneshot".into(),
            management: String::new(),
            purpose: String::new(),
            tags: String::new(),
            origin: String::new(),
            origin_scope: String::new(),
            state: String::new(),
            sub: String::new(),
            last_result: String::new(),
            last: String::new(),
            exec: String::new(),
            cwd: String::new(),
            fragments: String::new(),
            health_basis: String::new(),
            activation: "timer".into(),
        }
    }

    #[test]
    fn logs_default_to_service_unit() {
        assert_eq!(
            logs_unit("managed-speech-tts"),
            "managed-speech-tts.service"
        );
        assert_eq!(
            logs_unit("managed-proxy-health"),
            "managed-proxy-health.service"
        );
    }

    #[test]
    fn list_has_owned_and_watching_headers() {
        let rows = sectioned_rows(
            vec![dummy("managed-speech-tts", "owned")],
            vec![dummy("managed-proxy-health", "watching")],
        );
        let labels: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                ListRow::Header(h) => *h,
                ListRow::Op(op) => op.unit.as_str(),
            })
            .collect();
        assert_eq!(
            labels,
            [
                "OWNED",
                "managed-speech-tts",
                "WATCHING",
                "managed-proxy-health"
            ]
        );
    }

    #[test]
    fn empty_watching_is_omitted() {
        let rows = sectioned_rows(vec![dummy("managed-personal-wa-sync", "owned")], vec![]);
        let labels: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                ListRow::Header(h) => *h,
                ListRow::Op(op) => op.unit.as_str(),
            })
            .collect();
        assert_eq!(labels, ["OWNED", "managed-personal-wa-sync"]);
    }

    #[test]
    fn empty_owned_says_all_quiet() {
        let rows = sectioned_rows(vec![], vec![]);
        let labels: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                ListRow::Header(h) => *h,
                ListRow::Op(op) => op.unit.as_str(),
            })
            .collect();
        assert_eq!(labels, ["OWNED", "all quiet"]);
    }

    #[test]
    fn log_entries_unwrap_envelope() {
        let payload = serde_json::json!({
            "unit": "managed-x.service",
            "entries": [
                {"timestamp": "2026-08-22T08:55:07.000000Z", "message": "Started."},
                {"timestamp": "2026-08-22T08:55:08.000000Z", "message": "Connected."}
            ]
        });
        let entries = extract_log_entries(&payload);
        assert_eq!(entries.len(), 2);
        let line = format_log(&entries[1]);
        assert!(line.text.contains("Connected."));
        assert!(!line.text.contains("entries"));
        assert!(!line.alert);
    }

    #[test]
    fn log_alert_on_traceback() {
        let entry = serde_json::json!({
            "timestamp": "2026-08-22T08:56:25.000000Z",
            "message": "Traceback (most recent call last):"
        });
        let line = format_log(&entry);
        assert!(line.alert);
    }

    #[test]
    fn rfc3339_round_trip_secs() {
        let s = usec_to_rfc3339(1_582_934_400_123_456);
        assert_eq!(parse_rfc3339_utc(&s), Some(1_582_934_400));
    }

    #[test]
    fn relative_future_and_past() {
        assert_eq!(relative_to(1_000, 1_000 + 14 * 60), "in 14m");
        assert_eq!(relative_to(1_000, 1_000 - 3 * 60), "3m ago");
        assert_eq!(relative_to(1_000, 1_010), "now");
        assert_eq!(relative_to(0, 2 * 3_600), "in 2h");
    }

    #[test]
    fn tz_offset_and_clock() {
        assert_eq!(parse_tz_offset("+1000"), Some(10 * 3600));
        assert_eq!(parse_tz_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));
        // 08:55:07 UTC + 10h = 18:55:07
        assert_eq!(
            format_clock_at(
                parse_rfc3339_utc("2026-08-22T08:55:07.000000Z").unwrap(),
                10 * 3600
            ),
            "18:55:07"
        );
    }

    #[test]
    fn countdown_ticks_seconds() {
        assert_eq!(countdown_to(1_000, 1_000 + 4 * 60 + 12), "4m 12s");
        assert_eq!(countdown_to(1_000, 1_000 + 12), "12s");
        assert_eq!(countdown_to(1_000, 1_000), "now");
        assert_eq!(countdown_to(0, 2 * 3_600 + 3 * 60 + 5), "2h 03m 05s");
    }

    #[test]
    fn failed_when_uses_last() {
        let mut r = dummy("managed-personal-youtube-poll", "owned");
        r.health = "failed".into();
        r.last = usec_to_rfc3339(1_000_000 * 1_000);
        r.next = usec_to_rfc3339(1_000_000 * 2_000);
        let now = UNIX_EPOCH + Duration::from_secs(1_000 + 180);
        assert_eq!(when_label(&r, now), "3m ago");
    }

    #[test]
    fn running_simple_says_running() {
        let mut r = dummy("managed-personal-wa-sync", "owned");
        r.health = "healthy".into();
        r.kind = "simple".into();
        r.activation = "direct".into();
        r.sub = "running".into();
        assert_eq!(when_label(&r, SystemTime::now()), "running");
    }

    #[test]
    fn short_exec_uses_basename() {
        assert_eq!(
            short_exec("/home/sf/worlds/personal/.omp/bin/wa-self harvest"),
            "wa-self harvest"
        );
        assert_eq!(
            short_exec("/bin/bash -c /home/sf/worlds/personal/.omp/bin/yt-history poll && { /home/sf/worlds/personal/.omp/bin/yt-history llm-pass || true; }"),
            "yt-history poll && { yt-history llm-pass || true; }"
        );
    }
}
