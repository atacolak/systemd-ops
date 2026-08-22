//! Read-only operational console for one responsibility scope.
//!
//! Consumes [`crate::scope::show`]. Does not shell the CLI. Does not
//! mutate systemd.

use std::io::{self, stdout};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use ratatui::Terminal;
use serde_json::Value;

use crate::scope::{self, ScopeView};
use crate::systemd::{self, BackendError, LogFilter};

struct App {
    view: ScopeView,
    rows: Vec<ListRow>,
    list: ListState,
    filter: String,
    filtering: bool,
    logs: Vec<String>,
    last_error: Option<String>,
    last_refresh: Instant,
}

#[derive(Clone)]
enum ListRow {
    Header(&'static str),
    Op(Box<Row>),
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
        rows.push(ListRow::Header("(none)"));
    } else {
        rows.extend(owned.into_iter().map(|r| ListRow::Op(Box::new(r))));
    }
    rows.push(ListRow::Header("WATCHING"));
    if watching.is_empty() {
        rows.push(ListRow::Header("(none)"));
    } else {
        rows.extend(watching.into_iter().map(|r| ListRow::Op(Box::new(r))));
    }
    rows
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
        let mut watching = Vec::new();
        for v in &self.view.watching {
            if let Some(r) = row_from_view(v, "watching") {
                if self.filter_matches(&r) {
                    watching.push(r);
                }
            }
        }
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
            Ok(Value::Array(entries)) => {
                self.logs = entries.iter().map(format_log).collect();
                self.last_error = None;
            }
            Ok(other) => {
                self.logs = vec![other.to_string()];
            }
            Err(e) => {
                self.logs = vec![format!("logs unavailable: {e}")];
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
                .join(",")
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

fn format_log(entry: &Value) -> String {
    let ts = entry
        .get("timestamp")
        .or_else(|| entry.get("realtime"))
        .or_else(|| entry.get("__REALTIME_TIMESTAMP"))
        .map(|v| v.as_str().unwrap_or(&v.to_string()).to_string())
        .unwrap_or_default();
    let msg = entry
        .get("message")
        .or_else(|| entry.get("MESSAGE"))
        .map(|v| v.as_str().unwrap_or(&v.to_string()).to_string())
        .unwrap_or_else(|| entry.to_string());
    if ts.is_empty() {
        msg
    } else {
        format!("{ts}  {msg}")
    }
}

fn health_style(health: &str) -> Style {
    match health {
        "healthy" => Style::default().fg(Color::Green),
        "failed" => Style::default().fg(Color::Red),
        "degraded" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn mark(health: &str) -> &'static str {
    match health {
        "healthy" => "●",
        "failed" => "✖",
        _ => "?",
    }
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
            KeyCode::Char('l') | KeyCode::Enter => app.reload_logs(),
            _ => {}
        }
    }
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(8),
            Constraint::Length(10),
            Constraint::Length(1),
        ])
        .split(f.area());
    draw_header(f, chunks[0], app);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(chunks[1]);
    draw_list(f, body[0], app);
    draw_detail(f, body[1], app);
    draw_logs(f, chunks[2], app);
    draw_status(f, chunks[3], app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let id = app.view.id.to_uppercase();
    let health = app.view.health.as_str().to_uppercase();
    let n_owned = app.view.owned.len();
    let n_watch = app.view.watching.len();
    let n_att = app.view.attention.len();
    let root = app.view.root.display().to_string();
    let text = vec![
        Line::from(vec![
            Span::styled(
                format!("{id:<20}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(health, health_style(app.view.health.as_str())),
        ]),
        Line::from(root),
        Line::from(format!(
            "{n_owned} owned · {n_watch} watching · {n_att} attention"
        )),
    ];
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|r| match r {
            ListRow::Header(h) => {
                let style = if *h == "(none)" {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                };
                ListItem::new(Line::from(Span::styled(*h, style)))
            }
            ListRow::Op(r) => {
                let crit = if r.critical { "  crit" } else { "" };
                let next = if r.next.is_empty() {
                    String::new()
                } else {
                    format!("  {}", r.next)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", mark(&r.health)), health_style(&r.health)),
                    Span::raw(format!("{:<24} {:<8}{crit}{next}", r.title, r.health)),
                ]))
            }
        })
        .collect();
    let title = if app.filtering {
        format!("operations  /{}", app.filter)
    } else {
        "operations".into()
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = app.list.clone();
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let body = if let Some(r) = app.selected() {
        let crit = if r.critical { "yes" } else { "no" };
        vec![
            Line::from(format!("{}  ({})", r.title, r.unit)),
            Line::from(format!("purpose: {}", empty(&r.purpose))),
            Line::from(format!("tags: {}", empty(&r.tags))),
            Line::from(format!(
                "relationship: {}    critical: {crit}",
                r.relationship
            )),
            Line::from(format!("management: {}", empty(&r.management))),
            Line::from(format!(
                "health: {}  ({})  lifecycle is not functional proof",
                empty(&r.health),
                empty(&r.health_basis)
            )),
            Line::from(format!("state: {} / {}", empty(&r.state), empty(&r.sub))),
            Line::from(format!(
                "activation: {}    kind: {}",
                empty(&r.activation),
                empty(&r.kind)
            )),
            Line::from(format!("next: {}", empty(&r.next))),
            Line::from(format!(
                "last: {}    result: {}",
                empty(&r.last),
                empty(&r.last_result)
            )),
            Line::from(format!("exec: {}", empty(&r.exec))),
            Line::from(format!("cwd: {}", empty(&r.cwd))),
            Line::from(format!("origin cwd: {}", empty(&r.origin))),
            Line::from(format!("origin scope: {}", empty(&r.origin_scope))),
            Line::from(format!("fragments:\n{}", empty(&r.fragments))),
        ]
    } else {
        vec![Line::from("no operation selected")]
    };
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("detail")),
        area,
    );
}

fn draw_logs(f: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = if app.logs.is_empty() {
        vec![Line::from("no logs")]
    } else {
        app.logs.iter().map(|s| Line::from(s.as_str())).collect()
    };
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("logs")),
        area,
    );
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let msg = if let Some(err) = &app.last_error {
        err.clone()
    } else if app.filtering {
        format!("filter: {}  (enter apply, esc clear)", app.filter)
    } else {
        "q quit  j/k move  / filter  r refresh  enter/l reload logs  (read-only)".into()
    };
    f.render_widget(Paragraph::new(msg), area);
}

fn empty(s: &str) -> &str {
    if s.is_empty() {
        "\u{2014}"
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
