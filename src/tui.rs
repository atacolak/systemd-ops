//! Read-only operational console for one responsibility scope.
//!
//! Consumes [`crate::scope::show`]. Does not shell the CLI. Does not
//! mutate systemd. Default detail is the operator cockpit; wiring is
//! an alternate detail, topology is an author-written local file, and
//! diagnostics is an attached lazy drawer.
//!
//! Input and render stay on the terminal thread. Scope inspection and
//! journal reads run on a backend worker so a slow or hung systemctl
//! cannot freeze keyboard processing. The worker bounds its subprocess
//! waits; CLI and MCP waits stay unbounded unless a caller sets the
//! thread-local timeout.

use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEventKind,
};
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
const ACTIVITY_MAX: usize = 6;
const COCKPIT_ITERATIONS_MAX: usize = 5;

const BORDER: Color = Color::Rgb(86, 90, 108);
const MUTED: Color = Color::Rgb(138, 142, 158);
const TEXT: Color = Color::Rgb(220, 222, 228);
const ACCENT: Color = Color::Rgb(137, 196, 220);
const SEL_BG: Color = Color::Rgb(46, 50, 66);
const GREEN: Color = Color::Rgb(166, 209, 137);
const RED: Color = Color::Rgb(232, 136, 136);
const YELLOW: Color = Color::Rgb(230, 197, 120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailView {
    Cockpit,
    Wiring,
    Topology,
}

struct App {
    view: ScopeView,
    rows: Vec<ListRow>,
    list: ListState,
    filter: String,
    filtering: bool,
    logs: Vec<LogLine>,
    last_error: Option<String>,
    last_refresh: Instant,
    last_input: Instant,
    last_frame: Instant,
    last_backend_cmd: Option<String>,
    last_backend_elapsed: Option<Duration>,
    refreshing: bool,
    inflight_since: Option<Instant>,
    want_logs: bool,
    detail_view: DetailView,
    diagnostics_open: bool,
    detail_scroll: u16,
    detail_max_scroll: u16,
    detail_page: u16,
    log_scroll: u16,
    log_max_scroll: u16,
    list_area: Rect,
    detail_area: Rect,
    logs_area: Rect,
}

#[derive(Clone)]
enum ListRow {
    Header(&'static str),
    Op(Box<Row>),
}

#[derive(Clone, Debug)]
struct LogLine {
    text: String,
    alert: bool,
}

#[derive(Clone)]
struct ActivityEntry {
    at: String,
    text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveIteration {
    id: String,
    started_at: String,
    observed_updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Iteration {
    id: String,
    started_at: String,
    finished_at: String,
    exit_code: Option<i64>,
    reconsolidated: bool,
    headline: String,
    summary: String,
    outcome: String,
    route: String,
}

#[derive(Clone)]
struct Row {
    unit: String,
    title: String,
    health: String,
    semantic_state: String,
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
    schedule: String,
    scope_id: String,
    scope_root: String,
    operation_home: String,
    health_basis: String,
    activation: String,
    about: String,
    headline: String,
    body: String,
    updated_at: String,
    operator_state: String,
    basis_revision: String,
    active_iteration: Option<ActiveIteration>,
    iterations: Vec<Iteration>,
    activity: Vec<ActivityEntry>,
    definition_revision: String,
    agent: String,
    agent_root: String,
    parent: String,
    lifecycle: String,
    brain_revision: String,
    processed: String,
    checkpoint_generation: String,
    checkpoint_output: String,
    blocker: String,
    depth: usize,
    last_child: bool,
    tree_prefix: String,
}

pub(crate) fn logs_unit(stem: &str) -> String {
    format!("{stem}.service")
}

fn sectioned_rows(owned: Vec<Row>, watching: Vec<Row>) -> Vec<ListRow> {
    let mut rows = Vec::new();
    if watching.is_empty() {
        if owned.is_empty() {
            rows.push(ListRow::Header("all quiet"));
        } else {
            rows.extend(owned.into_iter().map(|r| ListRow::Op(Box::new(r))));
        }
        return rows;
    }
    rows.push(ListRow::Header("OWNED"));
    rows.extend(owned.into_iter().map(|r| ListRow::Op(Box::new(r))));
    rows.push(ListRow::Header("WATCHING"));
    rows.extend(watching.into_iter().map(|r| ListRow::Op(Box::new(r))));
    rows
}

fn hierarchy_rows(rows: Vec<Row>) -> Vec<Row> {
    use std::collections::{BTreeMap, BTreeSet};

    let units: BTreeSet<String> = rows.iter().map(|row| row.unit.clone()).collect();
    let mut by_unit: BTreeMap<String, Row> = rows
        .into_iter()
        .map(|row| (row.unit.clone(), row))
        .collect();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut roots = Vec::new();
    for row in by_unit.values() {
        if !row.parent.is_empty() && units.contains(&row.parent) {
            children
                .entry(row.parent.clone())
                .or_default()
                .push(row.unit.clone());
        } else {
            roots.push(row.unit.clone());
        }
    }
    let order = |left: &String, right: &String, values: &BTreeMap<String, Row>| {
        let a = values.get(left).expect("hierarchy unit");
        let b = values.get(right).expect("hierarchy unit");
        a.title.cmp(&b.title).then_with(|| a.unit.cmp(&b.unit))
    };
    roots.sort_by(|a, b| order(a, b, &by_unit));
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| order(a, b, &by_unit));
    }

    fn visit(
        unit: &str,
        depth: usize,
        prefix: String,
        last_child: bool,
        by_unit: &mut BTreeMap<String, Row>,
        children: &BTreeMap<String, Vec<String>>,
        output: &mut Vec<Row>,
    ) {
        let Some(mut row) = by_unit.remove(unit) else {
            return;
        };
        row.depth = depth;
        row.last_child = last_child;
        row.tree_prefix = prefix.clone();
        output.push(row);
        if let Some(siblings) = children.get(unit) {
            let last = siblings.len().saturating_sub(1);
            for (index, child) in siblings.iter().enumerate() {
                let child_last = index == last;
                let child_prefix = if depth == 0 {
                    String::new()
                } else {
                    format!("{}{}", prefix, if last_child { "  " } else { "│ " })
                };

                visit(
                    child,
                    depth + 1,
                    child_prefix,
                    child_last,
                    by_unit,
                    children,
                    output,
                );
            }
        }
    }

    let mut output = Vec::new();
    let last_root = roots.len().saturating_sub(1);
    for (index, root) in roots.iter().enumerate() {
        visit(
            root,
            0,
            String::new(),
            index == last_root,
            &mut by_unit,
            &children,
            &mut output,
        );
    }
    for (_, mut row) in by_unit {
        row.depth = 0;
        row.last_child = true;
        row.tree_prefix = String::new();
        output.push(row);
    }
    output
}

impl App {
    #[allow(dead_code)]
    fn load(scope_root: Option<&str>, cwd: Option<&str>) -> Result<Self, BackendError> {
        let view = scope::show_resolved(scope_root, cwd)?;
        Ok(Self::from_view(view))
    }

    fn from_view(view: ScopeView) -> Self {
        let mut app = Self {
            rows: Vec::new(),
            view,
            list: ListState::default(),
            filter: String::new(),
            filtering: false,
            logs: Vec::new(),
            last_error: None,
            last_refresh: Instant::now(),
            last_input: Instant::now(),
            last_frame: Instant::now(),
            last_backend_cmd: None,
            last_backend_elapsed: None,
            refreshing: false,
            inflight_since: None,
            want_logs: false,
            detail_view: DetailView::Cockpit,
            diagnostics_open: false,
            detail_scroll: 0,
            detail_max_scroll: 0,
            detail_page: 1,
            log_scroll: 0,
            log_max_scroll: 0,
            list_area: Rect::default(),
            detail_area: Rect::default(),
            logs_area: Rect::default(),
        };
        app.rebuild_rows();
        // Cockpit default: never fetch journald on load.
        app
    }

    fn rebuild_rows(&mut self) {
        let keep = self.selected().map(|r| r.unit.clone());
        let previous_scroll = self.detail_scroll;
        let mut owned = Vec::new();
        for v in &self.view.owned {
            if let Some(r) = row_from_view(v, "owned") {
                if self.filter_matches(&r) {
                    owned.push(r);
                }
            }
        }
        owned = hierarchy_rows(owned);
        let mut watching = Vec::new();
        for v in &self.view.watching {
            if let Some(r) = row_from_view(v, "watching") {
                if self.filter_matches(&r) {
                    watching.push(r);
                }
            }
        }
        watching.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.unit.cmp(&b.unit)));

        self.rows = sectioned_rows(owned, watching);
        if let Some(unit) = keep {
            if let Some(i) = self.rows.iter().position(|r| match r {
                ListRow::Op(op) => op.unit == unit,
                ListRow::Header(_) => false,
            }) {
                self.list.select(Some(i));
                self.detail_scroll = previous_scroll;
            } else {
                self.select_first_op();
                self.reset_detail_scroll();
            }
        } else {
            self.select_first_op();
            self.reset_detail_scroll();
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
            || row.about.to_ascii_lowercase().contains(&q)
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
                self.reset_detail_scroll();
                self.mark_logs_needed();
                return;
            }
        }
    }

    fn mark_logs_needed(&mut self) {
        if self.diagnostics_open {
            self.want_logs = true;
        }
    }

    fn apply_logs(&mut self, logs: Vec<LogLine>, error: Option<String>) {
        self.logs = logs;
        if let Some(error) = error {
            if self.logs.is_empty() {
                self.logs = vec![LogLine {
                    text: format!("logs unavailable: {error}"),
                    alert: true,
                }];
            }
        }
        self.log_scroll = 0;
    }

    fn selected_log_unit(&self) -> Option<String> {
        self.selected().map(|row| logs_unit(&row.unit))
    }

    fn reset_detail_scroll(&mut self) {
        self.detail_scroll = 0;
        self.detail_max_scroll = 0;
    }

    fn set_detail_extent(&mut self, content_lines: usize, visible_lines: usize) {
        self.detail_page = visible_lines.max(1).min(u16::MAX as usize) as u16;
        self.detail_max_scroll = content_lines
            .saturating_sub(visible_lines)
            .min(u16::MAX as usize) as u16;
        self.detail_scroll = self.detail_scroll.min(self.detail_max_scroll);
    }

    fn detail_page_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(self.detail_page.max(1));
    }

    fn detail_page_down(&mut self) {
        self.detail_scroll = self
            .detail_scroll
            .saturating_add(self.detail_page.max(1))
            .min(self.detail_max_scroll);
    }

    fn detail_home(&mut self) {
        self.detail_scroll = 0;
    }

    fn detail_end(&mut self) {
        self.detail_scroll = self.detail_max_scroll;
    }

    fn replace_view(&mut self, view: ScopeView) {
        self.view = view;
        self.last_error = None;
        self.rebuild_rows();
        self.mark_logs_needed();
        self.last_refresh = Instant::now();
    }

    fn toggle_wiring(&mut self) {
        self.detail_view = match self.detail_view {
            DetailView::Wiring => DetailView::Cockpit,
            DetailView::Cockpit | DetailView::Topology => DetailView::Wiring,
        };
        self.reset_detail_scroll();
    }

    fn toggle_topology(&mut self) {
        self.detail_view = match self.detail_view {
            DetailView::Topology => DetailView::Cockpit,
            DetailView::Cockpit | DetailView::Wiring => DetailView::Topology,
        };
        self.reset_detail_scroll();
    }

    fn toggle_diagnostics(&mut self) {
        self.diagnostics_open = !self.diagnostics_open;
        self.log_scroll = 0;
        self.log_max_scroll = 0;
        if self.diagnostics_open {
            self.mark_logs_needed();
        } else {
            self.logs.clear();
            self.want_logs = false;
        }
    }

    fn return_cockpit(&mut self) -> bool {
        if self.diagnostics_open {
            self.toggle_diagnostics();
            return true;
        }
        if matches!(self.detail_view, DetailView::Wiring | DetailView::Topology) {
            self.detail_view = DetailView::Cockpit;
            self.reset_detail_scroll();
            return true;
        }
        false
    }

    fn scroll_logs(&mut self, delta: i16) {
        self.log_scroll = if delta < 0 {
            self.log_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.log_scroll
                .saturating_add(delta as u16)
                .min(self.log_max_scroll)
        };
    }

    fn scroll_detail(&mut self, delta: i16) {
        self.detail_scroll = if delta < 0 {
            self.detail_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.detail_scroll
                .saturating_add(delta as u16)
                .min(self.detail_max_scroll)
        };
    }

    fn handle_mouse_scroll(&mut self, column: u16, row: u16, delta: i16) {
        if contains(self.list_area, column, row) {
            self.move_sel(if delta < 0 { -1 } else { 1 });
        } else if self.diagnostics_open && contains(self.logs_area, column, row) {
            self.scroll_logs(delta);
        } else if contains(self.detail_area, column, row) {
            self.scroll_detail(delta);
        }
    }
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
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
    let schedule = v
        .get("schedule")
        .filter(|value| !value.is_null())
        .and_then(schedule_text)
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
    let operator = v.get("operator");
    let about = operator
        .and_then(|o| o.get("about"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let headline = operator
        .and_then(|o| o.get("headline"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let body = operator
        .and_then(|o| o.get("body"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let updated_at = operator
        .and_then(|o| o.get("updated_at"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let active_iteration = operator
        .and_then(|o| o.get("active_iteration"))
        .and_then(|item| {
            if item.is_null() {
                return None;
            }
            Some(ActiveIteration {
                id: json_str(item, "id"),
                started_at: json_str(item, "started_at"),
                observed_updated_at: json_str(item, "observed_updated_at"),
            })
        });
    let mut iterations: Vec<Iteration> = operator
        .and_then(|o| o.get("iterations"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| Iteration {
                    id: json_str(item, "id"),
                    started_at: json_str(item, "started_at"),
                    finished_at: json_str(item, "finished_at"),
                    exit_code: item.get("exit_code").and_then(Value::as_i64),
                    reconsolidated: item
                        .get("reconsolidated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    headline: json_str(item, "headline"),
                    summary: json_str(item, "summary"),
                    outcome: json_str(item, "outcome"),
                    route: json_str(item, "route"),
                })
                .collect()
        })
        .unwrap_or_default();
    iterations.sort_by(|a, b| iteration_stamp(b).cmp(iteration_stamp(a)));
    iterations.truncate(20);
    let activity = operator
        .and_then(|o| o.get("activity"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let at = item.get("at")?.as_str()?.to_string();
                    let text = item.get("text")?.as_str()?.to_string();
                    Some(ActivityEntry { at, text })
                })
                .collect()
        })
        .unwrap_or_default();
    let operator_state = match v.get("operator_state") {
        Some(Value::Null) | None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    let automation = v.get("automation");
    let agent = automation
        .and_then(|value| value.get("agent"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let agent_root = automation
        .and_then(|value| value.get("agent_root"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let parent = automation
        .and_then(|value| value.get("parent"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let lifecycle = automation
        .and_then(|value| value.get("lifecycle"))
        .and_then(|value| value.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("active")
        .to_string();
    let brain_revision = automation
        .and_then(|value| value.get("brain_revision"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let processed = automation
        .and_then(|value| value.get("processed"))
        .and_then(|value| {
            value
                .get("input_fingerprint")
                .or_else(|| value.get("fingerprint"))
        })
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let semantic_state = automation
        .and_then(|value| value.get("semantic_state"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let checkpoint = automation.and_then(|value| value.get("checkpoint"));
    let checkpoint_generation = checkpoint
        .and_then(|value| value.get("generation"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let checkpoint_output = checkpoint
        .and_then(|value| value.get("output_revision"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let blocker = automation
        .and_then(|value| value.get("blocker"))
        .and_then(|value| {
            if value.is_null() {
                return None;
            }
            let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
            let summary = value.get("summary").and_then(Value::as_str).unwrap_or("");
            match (kind.is_empty(), summary.is_empty()) {
                (true, true) => None,
                (false, true) => Some(kind.to_string()),
                (true, false) => Some(summary.to_string()),
                (false, false) => Some(format!("{kind}: {summary}")),
            }
        })
        .unwrap_or_default();
    Some(Row {
        unit,
        title,
        health: json_str(v, "health"),
        semantic_state,
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
        schedule,
        scope_id: json_str(v, "scope_id"),
        scope_root: json_str(v, "scope_root"),
        operation_home: json_str(v, "operation_home"),
        cwd: json_str(v, "cwd"),
        fragments,
        health_basis: json_str(v, "health_basis"),
        activation: json_str(v, "activation"),
        about,
        headline,
        body,
        updated_at,
        operator_state,
        basis_revision: json_str(v, "basis_revision"),
        active_iteration,
        iterations,
        activity,
        definition_revision: json_str(v, "definition_revision"),
        agent,
        agent_root,
        parent,
        lifecycle,
        brain_revision,
        processed,
        checkpoint_generation,
        checkpoint_output,
        blocker,
        depth: 0,
        last_child: false,
        tree_prefix: String::new(),
    })
}

fn json_str(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::Null) | None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn schedule_text(value: &Value) -> Option<String> {
    let obj = value.as_object()?;
    match obj.get("type").and_then(Value::as_str)? {
        "calendar" => obj
            .get("on_calendar")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        "interval" => {
            let boot = obj
                .get("on_boot_sec")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let active = obj
                .get("on_unit_active_sec")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let inactive = obj
                .get("on_unit_inactive_sec")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            match (boot, active, inactive) {
                (Some(boot), Some(active), Some(inactive)) => {
                    Some(format!("boot {boot} · every {active} · idle {inactive}"))
                }
                (Some(boot), Some(active), None) => Some(format!("boot {boot} · every {active}")),
                (Some(boot), None, Some(inactive)) => {
                    Some(format!("boot {boot} · idle {inactive}"))
                }
                (None, Some(active), Some(inactive)) => {
                    Some(format!("every {active} · idle {inactive}"))
                }
                (Some(boot), None, None) => Some(format!("boot {boot}")),
                (None, Some(active), None) => Some(format!("every {active}")),
                (None, None, Some(inactive)) => Some(format!("idle {inactive}")),
                (None, None, None) => None,
            }
        }
        _ => None,
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

fn semantic_style(semantic: &str, health: &str) -> Style {
    match semantic {
        "ready" => Style::default().fg(GREEN),
        "blocked" => Style::default().fg(RED),
        "running" | "waiting" | "stale" => Style::default().fg(YELLOW),
        "neutral" => Style::default().fg(MUTED),
        _ => health_style(health),
    }
}

fn mark(semantic: &str, health: &str, lifecycle: &str) -> &'static str {
    if lifecycle == "completed" {
        return "✓";
    }
    if semantic.is_empty() {
        return match health {
            "healthy" => "●",
            "failed" => "✖",
            "unknown" => "?",
            _ => "○",
        };
    }
    match semantic {
        "ready" => "●",
        "running" => "▶",
        "waiting" => "⏳",
        "stale" => "◌",
        "blocked" => "■",
        "neutral" => "○",
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

const BACKEND_TIMEOUT: Duration = Duration::from_secs(8);
const HUNG_AFTER: Duration = Duration::from_secs(2);
const POLL_IDLE: Duration = Duration::from_millis(50);

#[derive(Clone, Debug, PartialEq, Eq)]
enum BackendJob {
    Refresh {
        gen: u64,
        scope_root: Option<String>,
        cwd: Option<String>,
    },
    Logs {
        gen: u64,
        unit: String,
    },
}

#[derive(Debug)]
enum BackendEvent {
    RefreshDone {
        gen: u64,
        result: Result<ScopeView, String>,
        elapsed: Duration,
        cmd: String,
    },
    LogsDone {
        gen: u64,
        unit: String,
        logs: Vec<LogLine>,
        error: Option<String>,
        elapsed: Duration,
    },
}

#[derive(Debug, Default)]
struct BackendCtl {
    refresh_wanted: u64,
    refresh_inflight: Option<u64>,
    logs_wanted: u64,
    logs_inflight: Option<u64>,
    pending_refresh: bool,
    pending_logs: Option<String>,
    busy: bool,
}

impl BackendCtl {
    fn request_refresh(&mut self) -> Option<u64> {
        // Coalesce: one extra generation is enough. Re-bumping while a
        // refresh is already pending would make the in-flight result stale
        // on every timer tick and the snapshot would never apply.
        if self.pending_refresh {
            return None;
        }
        self.refresh_wanted += 1;
        self.pending_refresh = true;
        self.try_dispatch_refresh()
    }

    fn request_logs(&mut self, unit: String) -> Option<(u64, String)> {
        self.logs_wanted += 1;
        self.pending_logs = Some(unit);
        self.try_dispatch_logs()
    }

    fn try_dispatch_refresh(&mut self) -> Option<u64> {
        if self.busy || !self.pending_refresh {
            return None;
        }
        self.pending_refresh = false;
        self.busy = true;
        self.refresh_inflight = Some(self.refresh_wanted);
        Some(self.refresh_wanted)
    }

    fn try_dispatch_logs(&mut self) -> Option<(u64, String)> {
        if self.busy || self.pending_refresh {
            return None;
        }
        let unit = self.pending_logs.take()?;
        self.busy = true;
        self.logs_inflight = Some(self.logs_wanted);
        Some((self.logs_wanted, unit))
    }

    fn try_dispatch_any(&mut self) -> Option<BackendJob> {
        if let Some(gen) = self.try_dispatch_refresh() {
            return Some(BackendJob::Refresh {
                gen,
                scope_root: None,
                cwd: None,
            });
        }
        self.try_dispatch_logs()
            .map(|(gen, unit)| BackendJob::Logs { gen, unit })
    }

    fn finish_refresh(&mut self, gen: u64) -> (bool, Option<BackendJob>) {
        if self.refresh_inflight != Some(gen) {
            return (false, self.try_dispatch_any());
        }
        self.refresh_inflight = None;
        self.busy = false;
        let apply = gen == self.refresh_wanted;
        (apply, self.try_dispatch_any())
    }

    fn finish_logs(&mut self, gen: u64) -> (bool, Option<BackendJob>) {
        if self.logs_inflight != Some(gen) {
            return (false, self.try_dispatch_any());
        }
        self.logs_inflight = None;
        self.busy = false;
        let apply = gen == self.logs_wanted;
        (apply, self.try_dispatch_any())
    }

    fn inflight_refresh(&self) -> bool {
        self.refresh_inflight.is_some() || self.pending_refresh
    }
}

struct WorkerCfg {
    manager: systemd::Manager,
    surface: systemd::Surface,
    write_prefix: Option<String>,
    timeout: Duration,
}

fn tui_trace(event: &str, detail: &str) {
    static SINK: LazyLock<Option<String>> =
        LazyLock::new(|| match std::env::var("SYSTEMD_OPS_TUI_TRACE") {
            Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => Some("-".into()),
            Ok(v) if !v.is_empty() => Some(v),
            _ => None,
        });
    let Some(sink) = SINK.as_ref() else {
        return;
    };
    let now = Instant::now();
    let line = format!("tui {event} {detail} mono={now:?}\n");
    if sink == "-" {
        eprint!("{line}");
        return;
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sink)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(line.as_bytes())
        });
}

fn loading_view() -> ScopeView {
    ScopeView {
        id: "loading".into(),
        automation_agent_root: None,
        coordination_lead: None,
        root: PathBuf::from("."),
        health: crate::scope::ScopeHealth::Unknown,
        owned: Vec::new(),
        watching: Vec::new(),
        attention: Vec::new(),
        warnings: vec!["loading scope".into()],
    }
}

pub fn run(scope_root: Option<&str>, cwd: Option<&str>) -> Result<(), BackendError> {
    enable_raw_mode().map_err(|e| BackendError(format!("tui: {e}")))?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .map_err(|e| BackendError(format!("tui: {e}")))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| BackendError(format!("tui: {e}")))?;
    let result = event_loop(&mut terminal, scope_root, cwd);
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .ok();
    result
}

fn handle_detail_navigation(app: &mut App, key: &KeyCode) -> bool {
    match key {
        KeyCode::PageUp => app.detail_page_up(),
        KeyCode::PageDown => app.detail_page_down(),
        KeyCode::Home => app.detail_home(),
        KeyCode::End => app.detail_end(),
        _ => return false,
    }
    true
}

struct TuiRuntime {
    app: App,
    gate: BackendCtl,
    jobs: Sender<BackendJob>,
    events: Receiver<BackendEvent>,
    scope_root: Option<String>,
    cwd: Option<String>,
}

impl TuiRuntime {
    fn dispatch(&mut self, job: BackendJob) {
        let job = match job {
            BackendJob::Refresh { gen, .. } => BackendJob::Refresh {
                gen,
                scope_root: self.scope_root.clone(),
                cwd: self.cwd.clone(),
            },
            other => other,
        };
        tui_trace("dispatch", &format!("{job:?}"));
        let _ = self.jobs.send(job);
        self.app.refreshing = self.gate.inflight_refresh() || self.gate.busy;
        if self.app.inflight_since.is_none() {
            self.app.inflight_since = Some(Instant::now());
        }
    }

    fn request_refresh(&mut self) {
        tui_trace("refresh_request", "");
        if let Some(gen) = self.gate.request_refresh() {
            self.dispatch(BackendJob::Refresh {
                gen,
                scope_root: None,
                cwd: None,
            });
        } else {
            self.app.refreshing = true;
        }
    }

    fn maybe_auto_refresh(&mut self) {
        if self.gate.inflight_refresh() || self.gate.busy {
            return;
        }
        if self.app.last_refresh.elapsed() >= REFRESH_EVERY {
            tui_trace("auto_refresh", "");
            self.request_refresh();
        }
    }

    fn maybe_request_logs(&mut self) {
        if !self.app.want_logs {
            return;
        }
        self.app.want_logs = false;
        if !self.app.diagnostics_open {
            return;
        }
        let Some(unit) = self.app.selected_log_unit() else {
            self.app.logs.clear();
            return;
        };
        if let Some((gen, unit)) = self.gate.request_logs(unit) {
            self.dispatch(BackendJob::Logs { gen, unit });
        }
    }

    fn drain_events(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(ev) => self.apply_event(ev),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.app.last_error = Some("tui backend worker exited".into());
                    break;
                }
            }
        }
        self.app.refreshing = self.gate.inflight_refresh() || self.gate.busy;
        if !self.gate.busy {
            self.app.inflight_since = None;
        }
        self.maybe_request_logs();
    }

    fn apply_event(&mut self, ev: BackendEvent) {
        match ev {
            BackendEvent::RefreshDone {
                gen,
                result,
                elapsed,
                cmd,
            } => {
                tui_trace(
                    "refresh_done",
                    &format!(
                        "gen={gen} elapsed_ms={} ok={}",
                        elapsed.as_millis(),
                        result.is_ok()
                    ),
                );
                self.app.last_backend_cmd = Some(cmd);
                self.app.last_backend_elapsed = Some(elapsed);
                let (apply, next) = self.gate.finish_refresh(gen);
                if apply {
                    match result {
                        Ok(view) => self.app.replace_view(view),
                        Err(e) => {
                            self.app.last_error = Some(e);
                            self.app.last_refresh = Instant::now();
                        }
                    }
                } else {
                    tui_trace(
                        "refresh_stale",
                        &format!("gen={gen} wanted={}", self.gate.refresh_wanted),
                    );
                }
                if let Some(job) = next {
                    self.dispatch(job);
                }
            }
            BackendEvent::LogsDone {
                gen,
                unit,
                logs,
                error,
                elapsed,
            } => {
                tui_trace(
                    "logs_done",
                    &format!(
                        "gen={gen} unit={unit} elapsed_ms={} err={}",
                        elapsed.as_millis(),
                        error.is_some()
                    ),
                );
                self.app.last_backend_elapsed = Some(elapsed);
                let (apply, next) = self.gate.finish_logs(gen);
                if apply && self.app.selected_log_unit().as_deref() == Some(unit.as_str()) {
                    self.app.apply_logs(logs, error);
                }
                if let Some(job) = next {
                    self.dispatch(job);
                }
            }
        }
    }
}

fn backend_worker(jobs: Receiver<BackendJob>, events: Sender<BackendEvent>, cfg: WorkerCfg) {
    systemd::set_manager(cfg.manager);
    systemd::set_surface(cfg.surface);
    systemd::set_write_prefix(cfg.write_prefix);
    systemd::set_proc_timeout(Some(cfg.timeout));
    while let Ok(job) = jobs.recv() {
        match job {
            BackendJob::Refresh {
                gen,
                scope_root,
                cwd,
            } => {
                tui_trace("refresh_start", &format!("gen={gen}"));
                let started = Instant::now();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    scope::show_resolved(scope_root.as_deref(), cwd.as_deref())
                }))
                .unwrap_or_else(|_| Err(BackendError("scope show panicked".into())))
                .map_err(|e| e.0);
                if events
                    .send(BackendEvent::RefreshDone {
                        gen,
                        result,
                        elapsed: started.elapsed(),
                        cmd: "scope show".into(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            BackendJob::Logs { gen, unit } => {
                tui_trace("logs_start", &format!("gen={gen} unit={unit}"));
                let started = Instant::now();
                let filter = LogFilter {
                    lines: 40,
                    priority: None,
                    since: None,
                    until: None,
                    boot: None,
                    grep: None,
                };
                let (logs, error) = match systemd::unit_logs(&unit, &filter) {
                    Ok(payload) => (
                        extract_log_entries(&payload)
                            .iter()
                            .map(format_log)
                            .collect(),
                        None,
                    ),
                    Err(e) => (Vec::new(), Some(e.0)),
                };
                if events
                    .send(BackendEvent::LogsDone {
                        gen,
                        unit,
                        logs,
                        error,
                        elapsed: started.elapsed(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    scope_root: Option<&str>,
    cwd: Option<&str>,
) -> Result<(), BackendError> {
    let (job_tx, job_rx) = mpsc::channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    let cfg = WorkerCfg {
        manager: systemd::manager(),
        surface: systemd::surface(),
        write_prefix: systemd::write_prefix().map(|g| g.join(",")),
        timeout: BACKEND_TIMEOUT,
    };
    std::thread::Builder::new()
        .name("systemd-ops-tui-backend".into())
        .spawn(move || backend_worker(job_rx, ev_tx, cfg))
        .map_err(|e| BackendError(format!("tui backend: {e}")))?;
    let mut rt = TuiRuntime {
        app: App::from_view(loading_view()),
        gate: BackendCtl::default(),
        jobs: job_tx,
        events: ev_rx,
        scope_root: scope_root.map(str::to_string),
        cwd: cwd.map(str::to_string),
    };
    rt.request_refresh();
    loop {
        rt.drain_events();
        rt.app.last_frame = Instant::now();
        terminal
            .draw(|f| draw(f, &mut rt.app))
            .map_err(|e| BackendError(format!("tui: {e}")))?;
        if !event::poll(POLL_IDLE).map_err(|e| BackendError(format!("tui: {e}")))? {
            rt.maybe_auto_refresh();
            continue;
        }
        match event::read().map_err(|e| BackendError(format!("tui: {e}")))? {
            Event::Mouse(mouse) => {
                let delta = match mouse.kind {
                    MouseEventKind::ScrollUp => -3,
                    MouseEventKind::ScrollDown => 3,
                    _ => continue,
                };
                rt.app.last_input = Instant::now();
                rt.app.handle_mouse_scroll(mouse.column, mouse.row, delta);
                rt.maybe_request_logs();
                continue;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                rt.app.last_input = Instant::now();
                tui_trace("input", &format!("{:?}", key.code));
                if rt.app.filtering {
                    match key.code {
                        KeyCode::Esc => {
                            rt.app.filtering = false;
                            rt.app.filter.clear();
                            rt.app.rebuild_rows();
                            rt.app.mark_logs_needed();
                        }
                        KeyCode::Enter => {
                            rt.app.filtering = false;
                            rt.app.rebuild_rows();
                            rt.app.mark_logs_needed();
                        }
                        KeyCode::Backspace => {
                            rt.app.filter.pop();
                            rt.app.rebuild_rows();
                        }
                        KeyCode::Char(c) => {
                            rt.app.filter.push(c);
                            rt.app.rebuild_rows();
                        }
                        _ => {}
                    }
                    rt.maybe_request_logs();
                    continue;
                }
                if handle_detail_navigation(&mut rt.app, &key.code) {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => {
                        tui_trace("quit", "requested");
                        return Ok(());
                    }
                    KeyCode::Esc => {
                        if !rt.app.return_cockpit() {
                            tui_trace("quit", "esc");
                            return Ok(());
                        }
                    }
                    KeyCode::Char('r') => rt.request_refresh(),
                    KeyCode::Char('/') => rt.app.filtering = true,
                    KeyCode::Down | KeyCode::Char('j') => rt.app.move_sel(1),
                    KeyCode::Up | KeyCode::Char('k') => rt.app.move_sel(-1),
                    KeyCode::Char('d') => rt.app.toggle_wiring(),
                    KeyCode::Char('t') => rt.app.toggle_topology(),
                    KeyCode::Char('l') => rt.app.toggle_diagnostics(),
                    _ => continue,
                }
                rt.maybe_request_logs();
            }
            _ => continue,
        }
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let header_h = 4;
    let status_h = 1;
    let total = f.area().height;
    let log_h = if app.diagnostics_open {
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
    let list_h = (app.rows.len() as u16)
        .saturating_add(2)
        .max(5)
        .min(body_h.saturating_sub(8));
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(list_h), Constraint::Min(8)])
        .split(chunks[1]);
    app.list_area = body[0];
    app.detail_area = body[1];
    app.logs_area = if log_h > 0 {
        chunks[2]
    } else {
        Rect::default()
    };
    draw_list(f, body[0], app);
    draw_detail(f, body[1], app);
    if log_h > 0 {
        draw_logs(f, chunks[2], app);
    }
    draw_status(f, chunks[3], app);
}

fn header_counts(view: &ScopeView) -> String {
    let n_owned = view.owned.len();
    let n_watch = view.watching.len();
    let n_att = view.attention.len();
    if n_watch == 0 {
        format!("{n_owned} owned · {n_att} attention")
    } else {
        format!("{n_owned} owned · {n_watch} watching · {n_att} attention")
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let id = app.view.id.to_uppercase();
    let health = app.view.health.as_str().to_uppercase();
    let counts = Span::styled(header_counts(&app.view), Style::default().fg(MUTED));
    let text = vec![
        Line::from(vec![
            Span::styled(id, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
            Span::styled("   ", Style::default()),
            Span::styled(
                health,
                health_style(app.view.health.as_str()).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(counts),
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
                let title = tree_title(r);
                let line = op_line(r, &title, &when, inner);

                ListItem::new(line)
            }
        })
        .collect();
    let title = if app.filtering {
        format!("AUTOMATIONS  /{}", app.filter)
    } else {
        "AUTOMATIONS".into()
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

fn tree_title(row: &Row) -> String {
    if row.depth == 0 {
        return row.title.clone();
    }
    let connector = if row.last_child { "└─" } else { "├─" };
    format!("{}{} {}", row.tree_prefix, connector, row.title)
}

fn op_line(row: &Row, title: &str, when: &str, inner: u16) -> Line<'static> {
    let mark = mark(&row.semantic_state, &row.health, &row.lifecycle);
    let failed = row.health == "failed";
    let mark_w = if failed { 4usize } else { 2usize };
    let when_w = when.chars().count();
    let budget = inner.saturating_sub(mark_w as u16 + 1) as usize;
    let title_budget = budget.saturating_sub(when_w);
    let title = truncate(title, title_budget);
    let used = mark_w + title.chars().count() + when_w;
    let pad = (inner as usize).saturating_sub(used);
    let state_style = if row.lifecycle == "completed" {
        Style::default().fg(ACCENT)
    } else {
        semantic_style(&row.semantic_state, &row.health)
    };
    let mut spans = vec![Span::styled(format!("{mark} "), state_style)];
    if failed {
        spans.push(Span::styled("✖ ", Style::default().fg(RED)));
    }
    spans.push(Span::styled(title, Style::default().fg(TEXT)));
    spans.push(Span::raw(" ".repeat(pad)));
    if !when.is_empty() {
        let when_style = if failed {
            Style::default().fg(RED)
        } else {
            Style::default().fg(MUTED)
        };
        spans.push(Span::styled(when.to_string(), when_style));
    }
    Line::from(spans)
}

fn description_for(r: &Row) -> &str {
    if !r.purpose.is_empty() {
        &r.purpose
    } else if !r.about.is_empty() {
        &r.about
    } else {
        &r.title
    }
}
fn operator_brief_style(state: &str) -> Style {
    match state {
        "outdated" | "error" | "unbased" => Style::default().fg(YELLOW),
        _ => Style::default().fg(MUTED),
    }
}

fn brief_heading(agent: &str) -> &'static str {
    if agent.is_empty() {
        "BRIEF"
    } else {
        "AGENT BRIEF"
    }
}

fn operator_brief_cue(state: &str) -> Option<&'static str> {
    match state {
        "outdated" => Some("outdated"),
        "missing" => Some("missing"),
        "error" => Some("error"),
        "unbased" => Some("unbased"),
        _ => None,
    }
}

fn activity_window(activity: &[ActivityEntry], max: usize) -> (usize, &[ActivityEntry]) {
    if activity.len() <= max {
        (0, activity)
    } else {
        let skip = activity.len() - max;
        (skip, &activity[skip..])
    }
}
fn cockpit_iterations(iterations: &[Iteration]) -> &[Iteration] {
    let end = iterations.len().min(COCKPIT_ITERATIONS_MAX);
    &iterations[..end]
}

fn iteration_stamp(iteration: &Iteration) -> &str {
    if iteration.finished_at.is_empty() {
        &iteration.started_at
    } else {
        &iteration.finished_at
    }
}

fn iteration_glyph(iteration: &Iteration) -> &'static str {
    if iteration.outcome == "blocked" {
        "⊘"
    } else {
        match iteration.exit_code {
            Some(0) => "✓",
            Some(_) => "✖",
            None => "?",
        }
    }
}

fn iteration_style(iteration: &Iteration) -> Style {
    if iteration.outcome == "blocked" {
        Style::default().fg(YELLOW)
    } else {
        match iteration.exit_code {
            Some(0) => Style::default().fg(GREEN),
            Some(_) => Style::default().fg(RED),
            None => Style::default().fg(YELLOW),
        }
    }
}

fn iteration_text(iteration: &Iteration) -> String {
    if iteration.exit_code.is_none() {
        "interrupted before producing a final brief".into()
    } else if !iteration.reconsolidated {
        "exited before reconsolidating a brief".into()
    } else if !iteration.headline.is_empty() {
        iteration.headline.clone()
    } else if iteration.outcome == "blocked" {
        "blocked".into()
    } else {
        match iteration.exit_code {
            Some(0) => "completed".into(),
            Some(code) => format!("exited {code}"),
            None => unreachable!(),
        }
    }
}

fn iteration_label(iteration: &Iteration, now: SystemTime) -> String {
    let when = iteration_stamp(iteration);
    let when = if when.is_empty() {
        String::new()
    } else {
        relative_label(when, now)
    };
    let text = iteration_text(iteration);
    if when.is_empty() {
        text
    } else {
        format!("{when}  {text}")
    }
}

fn active_iteration_label(iteration: &ActiveIteration, now: SystemTime) -> String {
    let when = if iteration.started_at.is_empty() {
        String::new()
    } else {
        relative_label(&iteration.started_at, now)
    };
    let mut text = String::from("iteration in progress");
    if !iteration.observed_updated_at.is_empty() {
        text.push_str(&format!(
            " · brief observed {}",
            relative_label(&iteration.observed_updated_at, now)
        ));
    }
    if when.is_empty() {
        text
    } else {
        format!("{when}  {text}")
    }
}

fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> usize {
    let width = width.max(1) as usize;
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
}

/// Plain cockpit surface for tests and review (no journal, no ExecStart).
#[cfg(test)]
fn cockpit_plain(r: &Row, now: SystemTime) -> String {
    let mut out = vec![description_for(r).to_string()];
    if r.relationship == "owned" {
        let mut brief = String::from(brief_heading(&r.agent));
        if !r.updated_at.is_empty() {
            brief.push_str(&format!("  {}", relative_label(&r.updated_at, now)));
        }
        if let Some(cue) = operator_brief_cue(&r.operator_state) {
            brief.push_str(&format!("  [{cue}]"));
        }
        out.push(brief);
        if !r.headline.is_empty() {
            out.push(r.headline.clone());
        }
        if !r.body.is_empty() {
            out.extend(r.body.split('\n').map(str::to_string));
        }

        if r.active_iteration.is_some() || !r.iterations.is_empty() {
            out.push("RECENT ITERATIONS".into());
            if let Some(active) = &r.active_iteration {
                out.push(format!("●  {}", active_iteration_label(active, now)));
            }
            for iteration in cockpit_iterations(&r.iterations) {
                out.push(format!(
                    "{}  {}",
                    iteration_glyph(iteration),
                    iteration_label(iteration, now)
                ));
            }
        }
        if !r.activity.is_empty() {
            out.push("NOTABLE ACTIVITY".into());
            let (earlier, window) = activity_window(&r.activity, ACTIVITY_MAX);
            if earlier > 0 {
                out.push(format!("… {earlier} earlier"));
            }
            for entry in window {
                let when = if entry.at.is_empty() {
                    String::new()
                } else {
                    relative_label(&entry.at, now)
                };
                out.push(if when.is_empty() {
                    entry.text.clone()
                } else {
                    format!("{when}  {}", entry.text)
                });
            }
        }
    }
    out.push("RUNTIME".into());
    out.push(format!("state  {}", state_line(r)));
    out.push(format!("last   {}", when_last(r, now)));
    out.push(format!("next   {}", next_display(r, now)));
    out.join("\n")
}

fn topology_path(root: &Path) -> PathBuf {
    root.join(".systemd-ops").join("topology.txt")
}

fn load_topology(root: &Path) -> Option<String> {
    std::fs::read_to_string(topology_path(root)).ok()
}

fn topology_detail_lines(text: Option<&str>) -> Vec<Line<'static>> {
    match text {
        None => vec![Line::from(Span::styled(
            "not authored yet",
            Style::default().fg(MUTED),
        ))],
        Some(body) => {
            let mut lines: Vec<Line<'static>> = body
                .lines()
                .map(|line| Line::from(Span::styled(line.to_string(), Style::default().fg(TEXT))))
                .collect();
            if body.ends_with('\n') {
                lines.push(Line::from(""));
            }
            if lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines
        }
    }
}

fn draw_detail(f: &mut Frame, area: Rect, app: &mut App) {
    if app.detail_view == DetailView::Topology {
        let body = topology_detail_lines(load_topology(&app.view.root).as_deref());
        let visible = area.height.saturating_sub(2) as usize;
        app.set_detail_extent(body.len(), visible);
        let paragraph = Paragraph::new(body).block(panel("TOPOLOGY"));
        f.render_widget(paragraph.scroll((app.detail_scroll, 0)), area);
        return;
    }
    let now = SystemTime::now();
    let body = if let Some(r) = app.selected() {
        match app.detail_view {
            DetailView::Wiring => wiring_detail_lines(r, now),
            DetailView::Cockpit => cockpit_detail_lines(r, now),
            DetailView::Topology => unreachable!(),
        }
    } else {
        vec![Line::from(Span::styled(
            "nothing selected",
            Style::default().fg(MUTED),
        ))]
    };
    let title = match app.detail_view {
        DetailView::Wiring => "wiring".into(),
        DetailView::Cockpit => app
            .selected()
            .map(|r| r.title.clone())
            .unwrap_or_else(|| "cockpit".into()),
        DetailView::Topology => "TOPOLOGY".into(),
    };
    let visible = area.height.saturating_sub(2) as usize;
    let width = area.width.saturating_sub(2).max(1);
    let content_lines = wrapped_line_count(&body, width);
    let paragraph = Paragraph::new(body)
        .wrap(Wrap { trim: true })
        .block(panel(title));
    app.set_detail_extent(content_lines, visible);
    f.render_widget(paragraph.scroll((app.detail_scroll, 0)), area);
}

fn section_heading(title: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        title,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))
}

fn cockpit_detail_lines(r: &Row, now: SystemTime) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        description_for(r).to_string(),
        Style::default().fg(MUTED),
    ))];
    if r.relationship == "owned" {
        lines.push(Line::from(""));
        let mut brief = vec![Span::styled(
            brief_heading(&r.agent),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )];
        if !r.updated_at.is_empty() {
            brief.push(Span::styled(
                format!("  {}", relative_label(&r.updated_at, now)),
                Style::default().fg(MUTED),
            ));
        }
        if let Some(cue) = operator_brief_cue(&r.operator_state) {
            brief.push(Span::styled(
                format!("  [{cue}]"),
                operator_brief_style(&r.operator_state),
            ));
        } else if r.operator_state == "current" {
            brief.push(Span::styled("  current", Style::default().fg(MUTED)));
        }
        lines.push(Line::from(brief));
        if !r.headline.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                r.headline.clone(),
                Style::default()
                    .fg(Color::Rgb(250, 248, 240))
                    .add_modifier(Modifier::BOLD),
            )));
        }
        if !r.body.is_empty() {
            lines.push(Line::from(""));
            lines.extend(
                r.body.split('\n').map(|line| {
                    Line::from(Span::styled(line.to_string(), Style::default().fg(TEXT)))
                }),
            );
        }

        if r.active_iteration.is_some() || !r.iterations.is_empty() {
            lines.push(Line::from(""));
            lines.push(section_heading("RECENT ITERATIONS"));
            if let Some(active) = &r.active_iteration {
                lines.push(Line::from(vec![
                    Span::styled("●  ", Style::default().fg(ACCENT)),
                    Span::styled(
                        active_iteration_label(active, now),
                        Style::default().fg(TEXT),
                    ),
                ]));
            }
            for iteration in cockpit_iterations(&r.iterations) {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{}  ", iteration_glyph(iteration)),
                        iteration_style(iteration),
                    ),
                    Span::styled(iteration_label(iteration, now), Style::default().fg(TEXT)),
                ]));
            }
        }

        if !r.activity.is_empty() {
            lines.push(Line::from(""));
            lines.push(section_heading("NOTABLE ACTIVITY"));
            let (earlier, window) = activity_window(&r.activity, ACTIVITY_MAX);
            if earlier > 0 {
                lines.push(Line::from(Span::styled(
                    format!("… {earlier} earlier"),
                    Style::default().fg(MUTED),
                )));
            }
            for entry in window {
                let when = if entry.at.is_empty() {
                    String::new()
                } else {
                    relative_label(&entry.at, now)
                };
                let text = if when.is_empty() {
                    entry.text.clone()
                } else {
                    format!("{when}  {}", entry.text)
                };
                lines.push(Line::from(Span::styled(text, Style::default().fg(TEXT))));
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(section_heading("RUNTIME"));
    lines.push(Line::from(vec![
        Span::styled("state ", Style::default().fg(MUTED)),
        Span::styled(state_line(r), Style::default().fg(TEXT)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("last  ", Style::default().fg(MUTED)),
        Span::styled(when_last(r, now), Style::default().fg(TEXT)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("next  ", Style::default().fg(MUTED)),
        Span::styled(next_display(r, now), Style::default().fg(TEXT)),
    ]));
    lines
}

fn wiring_detail_lines(r: &Row, _now: SystemTime) -> Vec<Line<'static>> {
    let mut lines = vec![section_heading("IDENTITY")];
    push_detail(&mut lines, "unit", r.unit.clone(), TEXT);
    push_detail(&mut lines, "title", r.title.clone(), TEXT);
    push_detail(&mut lines, "kind", r.kind.clone(), TEXT);
    push_detail(&mut lines, "mgmt", r.management.clone(), TEXT);

    lines.push(Line::from(""));
    lines.push(section_heading("RESPONSIBILITY"));
    push_detail(&mut lines, "relation", r.relationship.to_string(), TEXT);
    push_detail(&mut lines, "scope", r.scope_id.clone(), TEXT);
    push_detail(&mut lines, "root", short_path(&r.scope_root), MUTED);
    push_detail(&mut lines, "home", short_path(&r.operation_home), MUTED);
    push_detail(&mut lines, "purpose", r.purpose.clone(), TEXT);
    push_detail(&mut lines, "tags", r.tags.clone(), TEXT);
    push_detail(&mut lines, "brief", r.operator_state.clone(), TEXT);
    push_detail(&mut lines, "basis", r.basis_revision.clone(), MUTED);
    push_detail(&mut lines, "def", r.definition_revision.clone(), MUTED);
    push_detail(&mut lines, "agent", r.agent.clone(), TEXT);
    if let Some(active) = &r.active_iteration {
        push_detail(&mut lines, "iteration", active.id.clone(), TEXT);
    }
    for iteration in &r.iterations {
        let mut value = iteration.id.clone();
        if !iteration.outcome.is_empty() {
            value.push_str(&format!("  {}", iteration.outcome));
        }
        if !iteration.route.is_empty() {
            value.push_str(&format!("  {}", iteration.route));
        }
        push_detail(&mut lines, "iteration", value, MUTED);
    }
    push_detail(&mut lines, "agent root", short_path(&r.agent_root), MUTED);
    push_detail(&mut lines, "parent", r.parent.clone(), TEXT);

    push_detail(&mut lines, "lifecycle", r.lifecycle.clone(), TEXT);
    push_detail(&mut lines, "semantic", r.semantic_state.clone(), TEXT);

    push_detail(&mut lines, "brain", r.brain_revision.clone(), MUTED);
    push_detail(&mut lines, "processed", r.processed.clone(), MUTED);
    push_detail(
        &mut lines,
        "generation",
        r.checkpoint_generation.clone(),
        TEXT,
    );
    push_detail(&mut lines, "output", r.checkpoint_output.clone(), TEXT);
    push_detail(&mut lines, "blocker", r.blocker.clone(), TEXT);

    lines.push(Line::from(""));
    lines.push(section_heading("EXECUTION"));
    push_detail(&mut lines, "exec", short_exec(&r.exec), TEXT);
    push_detail(&mut lines, "cwd", short_path(&r.cwd), TEXT);
    for frag in r.fragments.lines().filter(|s| !s.is_empty()) {
        push_detail(&mut lines, "file", short_path(frag), MUTED);
    }
    push_detail(&mut lines, "origin", short_path(&r.origin), MUTED);
    push_detail(&mut lines, "source", r.origin_scope.clone(), MUTED);

    lines.push(Line::from(""));
    lines.push(section_heading("ACTIVATION"));
    push_detail(&mut lines, "type", r.activation.clone(), TEXT);
    push_detail(&mut lines, "schedule", r.schedule.clone(), TEXT);
    push_detail(&mut lines, "state", state_line(r), TEXT);
    push_detail(&mut lines, "sub", r.sub.clone(), TEXT);
    push_detail(&mut lines, "health", r.health_basis.clone(), MUTED);
    lines
}

fn push_detail(lines: &mut Vec<Line<'static>>, label: &str, value: String, color: Color) {
    if value.is_empty() {
        return;
    }
    lines.push(Line::from(vec![
        muted_k(label),
        Span::styled(value, Style::default().fg(color)),
    ]));
}

fn muted_k(label: &str) -> Span<'static> {
    Span::styled(format!("{label:<9}"), Style::default().fg(MUTED))
}

fn draw_logs(f: &mut Frame, area: Rect, app: &App) {
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
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .scroll((app.log_scroll, 0))
            .block(panel("logs")),
        area,
    );
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let hung = app
        .inflight_since
        .is_some_and(|started| started.elapsed() >= HUNG_AFTER);
    let msg = if let Some(err) = &app.last_error {
        Line::from(Span::styled(err.clone(), Style::default().fg(RED)))
    } else if app.filtering {
        Line::from(Span::styled(
            format!("filter  {}   enter apply · esc clear", app.filter),
            Style::default().fg(ACCENT),
        ))
    } else if hung {
        Line::from(Span::styled(
            "backend slow · showing last snapshot   q still quits",
            Style::default().fg(YELLOW),
        ))
    } else if app.refreshing {
        Line::from(Span::styled(
            "refreshing…   q quit   j/k still move",
            Style::default().fg(ACCENT),
        ))
    } else {
        let detail = match app.detail_view {
            DetailView::Cockpit => "cockpit",
            DetailView::Wiring => "wiring",
            DetailView::Topology => "topology",
        };
        let logs = if app.diagnostics_open { " + logs" } else { "" };
        Line::from(Span::styled(
            format!("q quit   j/k move   wheel/PgUp/PgDn scroll   Home/End   / find   r refresh   d wiring   t topology   l logs   · {detail}{logs}"),
            Style::default().fg(MUTED),
        ))
    };
    f.render_widget(Paragraph::new(msg), area);
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
    if r.lifecycle == "completed" {
        return "completed".into();
    }
    if r.health == "failed" {
        if !r.last.is_empty() {
            return relative_label(&r.last, now);
        }
        return "failed".into();
    }
    let intelligent_running = r.active_iteration.is_some()
        && (r.state == "active" || r.state == "activating")
        && (r.sub == "running" || r.sub == "start");
    if intelligent_running {
        return "running".into();
    }
    if (r.activation == "direct" || r.kind == "simple")
        && r.active_iteration.is_none()
        && !r.sub.is_empty()
        && r.sub != "dead"
        && r.sub != "exited"
        && r.sub != "running"
        && r.sub != "start"
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
    use crate::scope::{Attention, ScopeHealth};
    use crate::systemd::usec_to_rfc3339;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn dummy(unit: &str, relationship: &'static str) -> Row {
        Row {
            unit: unit.into(),
            title: unit.into(),
            health: "unknown".into(),
            semantic_state: String::new(),
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
            schedule: String::new(),
            scope_id: String::new(),
            scope_root: String::new(),
            operation_home: String::new(),
            activation: "timer".into(),
            about: String::new(),
            headline: String::new(),
            body: String::new(),
            updated_at: String::new(),
            operator_state: String::new(),
            active_iteration: None,
            iterations: Vec::new(),
            basis_revision: String::new(),
            activity: Vec::new(),
            definition_revision: String::new(),
            agent: String::new(),
            agent_root: String::new(),
            parent: String::new(),
            lifecycle: "active".into(),
            brain_revision: String::new(),
            processed: String::new(),
            checkpoint_generation: String::new(),
            checkpoint_output: String::new(),
            blocker: String::new(),
            depth: 0,
            last_child: false,
            tree_prefix: String::new(),
        }
    }

    fn empty_view() -> ScopeView {
        let mut view = ScopeView {
            id: "personal".into(),
            automation_agent_root: None,
            coordination_lead: None,
            root: PathBuf::from("/tmp/personal"),
            health: ScopeHealth::Healthy,
            owned: vec![serde_json::json!({
                "unit": "managed-personal-pr-maintainer",
                "title": "PR maintainer",
                "purpose": "fallback purpose",
                "health": "healthy",
                "kind": "oneshot",
                "activation": "timer",
                "state": "inactive",
                "sub": "dead",
                "last": "2026-08-22T10:00:00.000000Z",
                "next": "2026-08-22T12:00:00.000000Z",
                "last_result": "success",
                "exec": {"path": "/bin/bash", "argv": ["-c", "ExecStart=/usr/bin/true"]},
                "cwd": "/tmp",
                "tags": ["ops"],
                "management": "user",
                "fragment_paths": ["/tmp/x.service"],
                "origin_cwd": "/tmp",
                "origin_scope": "personal",
                "health_basis": "timer",
                "definition_revision": "sha256:abc",
                "critical": false,
                "automation": {
                    "agent": "pr-maintainer"
                },
                "operator": {
                    "version": 1,
                    "about": "Keeps PR queues honest",
                    "headline": "Queue drained",
                    "body": "No open PRs need attention.",
                    "updated_at": "2026-08-22T11:00:00.000000Z",
                    "basis_revision": "sha256:abc",
                    "active_iteration": {
                        "id": "iter-active",
                        "started_at": "2026-08-22T11:20:00.000000Z",
                        "observed_updated_at": "2026-08-22T11:00:00.000000Z"
                    },
                    "iterations": [
                        {
                            "id": "iter-failed",
                            "started_at": "2026-08-22T10:40:00.000000Z",
                            "finished_at": "2026-08-22T10:50:00.000000Z",
                            "exit_code": 2,
                            "reconsolidated": false,
                            "headline": null,
                            "summary": null
                        },
                        {
                            "id": "iter-ok",
                            "started_at": "2026-08-22T10:00:00.000000Z",
                            "finished_at": "2026-08-22T10:30:00.000000Z",
                            "exit_code": 0,
                            "reconsolidated": true,
                            "headline": "Queue drained",
                            "summary": "No open PRs"
                        },
                        {
                            "id": "iter-interrupted",
                            "started_at": "2026-08-22T09:10:00.000000Z",
                            "finished_at": null,
                            "exit_code": null,
                            "reconsolidated": false,
                            "headline": null,
                            "summary": null
                        }
                    ],
                    "activity": [
                        {"at": "2026-08-22T09:00:00.000000Z", "text": "earlier sweep"},
                        {"at": "2026-08-22T10:30:00.000000Z", "text": "closed stale PR"},
                        {"at": "2026-08-22T11:00:00.000000Z", "text": "queue empty"}
                    ]
                },
                "operator_state": "current"
            })],
            watching: vec![],
            attention: vec![],
            warnings: vec![],
        };
        view.owned[0]["scope_id"] = serde_json::json!("personal");
        view.owned[0]["scope_root"] = serde_json::json!("/tmp/personal");
        view.owned[0]["operation_home"] = serde_json::json!(
            "/tmp/personal/.systemd-ops/operations/managed-personal-pr-maintainer"
        );

        view.owned[0]["basis_revision"] = serde_json::json!("sha256:abc");
        view
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
        assert_eq!(labels, ["managed-personal-wa-sync"]);
    }

    #[test]
    fn owned_only_list_title_is_automations() {
        let mut app = App::from_view(empty_view());
        let backend = TestBackend::new(100, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("AUTOMATIONS"));
        assert!(!text.contains("OWNED"));
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
        assert_eq!(labels, ["all quiet"]);
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
    fn running_requires_active_iteration_and_service() {
        let mut r = dummy("managed-personal-wa-sync", "owned");
        r.health = "healthy".into();
        r.kind = "simple".into();
        r.activation = "direct".into();
        r.state = "active".into();
        r.sub = "running".into();
        assert_eq!(when_label(&r, SystemTime::now()), "");
        r.active_iteration = Some(ActiveIteration {
            id: "iteration-1".into(),
            started_at: String::new(),
            observed_updated_at: String::new(),
        });
        assert_eq!(when_label(&r, SystemTime::now()), "running");
        r.state = "activating".into();
        r.sub = "start".into();
        assert_eq!(when_label(&r, SystemTime::now()), "running");
        r.state = "inactive".into();
        assert_eq!(when_label(&r, SystemTime::now()), "");
    }

    #[test]
    fn hierarchy_orders_parent_before_children() {
        let mut parent = dummy("managed-parent", "owned");
        parent.title = "Parent".into();
        let mut child_a = dummy("managed-child-a", "owned");
        child_a.title = "A child".into();
        child_a.parent = parent.unit.clone();
        let mut child_b = dummy("managed-child-b", "owned");
        child_b.title = "B child".into();
        child_b.parent = parent.unit.clone();
        let rows = hierarchy_rows(vec![child_b, parent, child_a]);
        assert_eq!(
            rows.iter().map(|row| row.unit.as_str()).collect::<Vec<_>>(),
            vec!["managed-parent", "managed-child-a", "managed-child-b"]
        );
        assert_eq!(rows[0].depth, 0);
        assert_eq!(tree_title(&rows[1]), "├─ A child");
        assert_eq!(tree_title(&rows[2]), "└─ B child");
    }

    #[test]
    fn completed_row_is_distinct_and_idle() {
        let mut row = dummy("managed-completed", "owned");
        row.lifecycle = "completed".into();
        row.state = "inactive".into();
        row.sub = "dead".into();
        assert_eq!(mark(&row.semantic_state, &row.health, &row.lifecycle), "✓");

        assert_eq!(when_label(&row, SystemTime::now()), "completed");
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

    #[test]
    fn from_view_defaults_cockpit_without_logs() {
        let app = App::from_view(empty_view());
        assert_eq!(app.detail_view, DetailView::Cockpit);
        assert!(!app.diagnostics_open);
        assert!(app.logs.is_empty());
        assert!(app.selected().is_some());
    }

    #[test]
    fn selection_in_cockpit_does_not_fetch_logs() {
        let mut view = empty_view();
        view.owned.push(serde_json::json!({
            "unit": "managed-personal-other",
            "title": "Other",
            "health": "healthy",
            "kind": "oneshot",
            "activation": "timer",
            "operator": Value::Null,
            "operator_state": "missing"
        }));
        let mut app = App::from_view(view);
        assert!(app.logs.is_empty());
        app.move_sel(1);
        assert_eq!(app.detail_view, DetailView::Cockpit);
        assert!(!app.diagnostics_open);
        assert!(app.logs.is_empty());
        app.move_sel(-1);
        assert!(app.logs.is_empty());
    }

    #[test]
    fn diagnostics_toggle_clears_on_leave() {
        let mut app = App::from_view(empty_view());
        app.diagnostics_open = true;
        app.logs = vec![LogLine {
            text: "Traceback (most recent call last):".into(),
            alert: true,
        }];
        app.toggle_diagnostics();
        assert!(!app.diagnostics_open);
        assert_eq!(app.detail_view, DetailView::Cockpit);
        assert!(app.logs.is_empty());
    }

    #[test]
    fn diagnostics_drawer_keeps_detail_mode() {
        let mut app = App::from_view(empty_view());
        app.detail_view = DetailView::Wiring;
        app.diagnostics_open = true;
        app.logs = vec![LogLine {
            text: "diagnostic".into(),
            alert: false,
        }];
        app.toggle_diagnostics();
        assert!(!app.diagnostics_open);
        assert_eq!(app.detail_view, DetailView::Wiring);
        assert!(app.logs.is_empty());
    }

    #[test]
    fn diagnostics_drawer_retains_selected_detail() {
        let mut app = App::from_view(empty_view());
        app.diagnostics_open = true;
        app.logs = vec![LogLine {
            text: "diagnostic output".into(),
            alert: false,
        }];
        let backend = TestBackend::new(100, 34);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Queue drained"));
        assert!(text.contains("diagnostic output"));
    }

    #[test]
    fn wiring_toggle_round_trips() {
        let mut app = App::from_view(empty_view());
        app.toggle_wiring();
        assert_eq!(app.detail_view, DetailView::Wiring);
        app.toggle_wiring();
        assert_eq!(app.detail_view, DetailView::Cockpit);
        assert!(!app.return_cockpit());
        app.toggle_wiring();
        assert!(app.return_cockpit());
        assert_eq!(app.detail_view, DetailView::Cockpit);
    }
    #[test]
    fn cockpit_prefers_purpose_over_legacy_about() {
        let app = App::from_view(empty_view());
        let row = app.selected().unwrap();
        assert_eq!(description_for(row), "fallback purpose");
        let mut legacy = dummy("managed-proxy-health", "watching");
        legacy.about = "legacy proxy canary".into();
        assert_eq!(description_for(&legacy), "legacy proxy canary");
    }

    #[test]
    fn outdated_and_missing_briefs_are_advisory_not_red() {
        assert_eq!(operator_brief_cue("outdated"), Some("outdated"));
        assert_eq!(operator_brief_cue("missing"), Some("missing"));
        assert_eq!(operator_brief_cue("error"), Some("error"));
        assert_eq!(operator_brief_cue("current"), None);
        let outdated = operator_brief_style("outdated");
        assert_eq!(outdated.fg, Some(YELLOW));
        assert_ne!(outdated.fg, Some(RED));
        let missing = operator_brief_style("missing");
        assert_eq!(missing.fg, Some(MUTED));
        assert_ne!(missing.fg, Some(RED));
    }

    #[test]
    fn cockpit_preserves_summary_paragraph_breaks() {
        let mut app = App::from_view(empty_view());
        let row = match app.rows.iter_mut().find_map(|row| match row {
            ListRow::Op(row) => Some(row.as_mut()),
            ListRow::Header(_) => None,
        }) {
            Some(row) => row,
            None => panic!("missing row"),
        };
        row.body = "first paragraph\n\nsecond paragraph".into();
        let lines = cockpit_detail_lines(row, SystemTime::now());
        let plain = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        let first = plain
            .iter()
            .position(|line| line == "first paragraph")
            .unwrap();
        let second = plain
            .iter()
            .position(|line| line == "second paragraph")
            .unwrap();
        assert_eq!(second, first + 2);
        assert!(plain[first + 1].is_empty());
    }

    #[test]
    fn cockpit_styles_headline_above_summary() {
        let app = App::from_view(empty_view());
        let row = app.selected().unwrap();
        let lines = cockpit_detail_lines(row, SystemTime::now());
        let headline = lines
            .iter()
            .find(|line| line.to_string() == "Queue drained")
            .expect("headline line");
        let summary = lines
            .iter()
            .find(|line| line.to_string() == "No open PRs need attention.")
            .expect("summary line");
        assert!(headline.spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert!(!summary.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(headline.spans[0].style.fg, Some(Color::Rgb(250, 248, 240)));
        assert_eq!(summary.spans[0].style.fg, Some(TEXT));
    }

    #[test]
    fn wiring_surfaces_generic_responsibility_fields() {
        let app = App::from_view(empty_view());
        let row = app.selected().unwrap();
        let text = wiring_detail_lines(row, SystemTime::now())
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "IDENTITY",
            "RESPONSIBILITY",
            "EXECUTION",
            "ACTIVATION",
            "root",
            "home",
            "relation",
            "basis",
            "def",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn schedule_text_uses_only_typed_schedule_fields() {
        assert_eq!(
            schedule_text(&serde_json::json!({
                "type": "calendar",
                "on_calendar": "Mon..Fri 09:00",
                "persistent": true,
                "accuracy_sec": null
            })),
            Some("Mon..Fri 09:00".into())
        );
        assert_eq!(
            schedule_text(&serde_json::json!({
                "type": "interval",
                "on_boot_sec": "2min",
                "on_unit_active_sec": "15min",
                "persistent": false
            })),
            Some("boot 2min · every 15min".into())
        );
        assert_eq!(
            schedule_text(&serde_json::json!({
                "type": "interval",
                "on_boot_sec": "5min",
                "on_unit_inactive_sec": "3min",
                "persistent": false
            })),
            Some("boot 5min · idle 3min".into())
        );
        assert_eq!(
            schedule_text(&serde_json::json!({"type": "calendar"})),
            None
        );
        assert_eq!(
            schedule_text(&serde_json::json!({"type": "invented", "text": "daily"})),
            None
        );
    }

    #[test]
    fn mouse_wheel_routes_by_region() {
        let mut app = App::from_view(empty_view());
        app.list_area = Rect::new(0, 0, 20, 5);
        app.detail_area = Rect::new(0, 5, 20, 10);
        app.logs_area = Rect::new(0, 15, 20, 5);
        app.detail_max_scroll = 20;
        app.log_max_scroll = 20;
        app.diagnostics_open = true;

        app.handle_mouse_scroll(1, 6, 3);
        assert_eq!(app.detail_scroll, 3);
        app.handle_mouse_scroll(1, 16, 3);
        assert_eq!(app.log_scroll, 3);

        let mut view = empty_view();
        view.owned.push(serde_json::json!({
            "unit": "managed-personal-other",
            "title": "Other",
            "health": "healthy",
            "operator": null,
            "operator_state": "missing"
        }));
        let mut app = App::from_view(view);
        app.list_area = Rect::new(0, 0, 20, 5);
        let before = app.selected().unwrap().unit.clone();
        app.handle_mouse_scroll(1, 1, 3);
        assert_ne!(app.selected().unwrap().unit, before);
    }

    #[test]
    fn header_owned_watching_attention() {
        let mut view = empty_view();
        assert_eq!(header_counts(&view), "1 owned · 0 attention");
        view.watching.push(serde_json::json!({
            "unit": "managed-proxy-health",
            "title": "proxy",
            "health": "healthy"
        }));
        view.attention.push(Attention {
            operation: "managed-personal-pr-maintainer".into(),
            relationship: "owned",
            code: "operation_failed",
            reason: "failed".into(),
        });
        assert_eq!(header_counts(&view), "1 owned · 1 watching · 1 attention");
    }

    #[test]
    fn pr_maintainer_cockpit_surface_omits_exec_and_traceback() {
        let app = App::from_view(empty_view());
        let row = app.selected().unwrap();
        let now = UNIX_EPOCH
            + Duration::from_secs(parse_rfc3339_utc("2026-08-22T11:30:00Z").unwrap() as u64);
        let text = cockpit_plain(row, now);
        assert!(text.contains("Queue drained"));
        assert!(text.contains("NOTABLE ACTIVITY"));
        assert!(text.contains("queue empty"));
        assert!(text.contains("RUNTIME"));
        assert!(!text.contains("ExecStart"));
        assert!(!text.to_ascii_lowercase().contains("traceback"));
        assert!(!text.contains("/bin/bash"));
    }

    #[test]
    fn row_from_view_carries_operator_fields() {
        let app = App::from_view(empty_view());
        let row = app.selected().unwrap();
        assert_eq!(row.about, "Keeps PR queues honest");
        assert_eq!(row.headline, "Queue drained");
        assert_eq!(row.operator_state, "current");
        assert_eq!(row.activity.len(), 3);
        assert_eq!(row.active_iteration.as_ref().unwrap().id, "iter-active");
        assert_eq!(row.iterations.len(), 3);
        assert_eq!(row.iterations[0].id, "iter-failed");
        assert_eq!(row.definition_revision, "sha256:abc");
        assert_eq!(row.scope_id, "personal");
        assert_eq!(row.scope_root, "/tmp/personal");
        assert!(row
            .operation_home
            .ends_with("managed-personal-pr-maintainer"));
        assert_eq!(row.basis_revision, "sha256:abc");
        assert!(row.exec.contains("ExecStart") || row.exec.contains("/bin/bash"));
    }

    #[test]
    fn activity_truncation_marks_earlier() {
        let activity: Vec<ActivityEntry> = (0..8)
            .map(|i| ActivityEntry {
                at: format!("2026-08-22T10:0{i}:00.000000Z"),
                text: format!("step {i}"),
            })
            .collect();
        let (earlier, window) = activity_window(&activity, 6);
        assert_eq!(earlier, 2);
        assert_eq!(window.len(), 6);
        assert_eq!(window[0].text, "step 2");
    }

    #[test]
    fn cockpit_combines_sections_in_operator_order() {
        let app = App::from_view(empty_view());
        let row = app.selected().unwrap();
        let now = UNIX_EPOCH
            + Duration::from_secs(parse_rfc3339_utc("2026-08-22T11:30:00Z").unwrap() as u64);
        let text = cockpit_plain(row, now);
        let headings = [
            "fallback purpose",
            "AGENT BRIEF",
            "RECENT ITERATIONS",
            "NOTABLE ACTIVITY",
            "RUNTIME",
        ];
        let positions: Vec<_> = headings
            .iter()
            .map(|heading| text.find(heading).unwrap())
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!text.contains("\nACTIVITY\n"));
    }

    #[test]
    fn agentless_cockpit_uses_brief_not_agent_brief() {
        let mut view = empty_view();
        view.owned[0].as_object_mut().unwrap().remove("automation");
        let app = App::from_view(view);
        let row = app.selected().unwrap();
        let now = UNIX_EPOCH
            + Duration::from_secs(parse_rfc3339_utc("2026-08-22T11:30:00Z").unwrap() as u64);
        let text = cockpit_plain(row, now);
        assert!(text.contains("BRIEF"));
        assert!(!text.contains("AGENT BRIEF"));
        assert_eq!(brief_heading(""), "BRIEF");
        assert_eq!(brief_heading("pr-maintainer"), "AGENT BRIEF");
    }

    #[test]
    fn blocked_exit_zero_is_not_a_failed_wrapper() {
        let mut view = empty_view();
        let operator = view.owned[0].get_mut("operator").unwrap();
        operator["active_iteration"] = Value::Null;
        operator["iterations"] = serde_json::json!([{
            "id": "iter-blocked",
            "started_at": "2026-08-22T10:00:00Z",
            "finished_at": "2026-08-22T10:05:00Z",
            "exit_code": 0,
            "reconsolidated": true,
            "headline": "newer GitHub tag; upgrade belongs to the lead",
            "summary": "paged vase",
            "outcome": "blocked",
            "route": "lead"
        }]);
        let app = App::from_view(view);
        let row = app.selected().unwrap();
        let now = UNIX_EPOCH
            + Duration::from_secs(parse_rfc3339_utc("2026-08-22T11:30:00Z").unwrap() as u64);
        assert_eq!(row.iterations[0].outcome, "blocked");
        assert_eq!(row.iterations[0].route, "lead");
        assert_eq!(iteration_glyph(&row.iterations[0]), "⊘");
        let text = cockpit_plain(row, now);
        assert!(text.contains("⊘  1h ago  newer GitHub tag; upgrade belongs to the lead"));
        assert!(!text.contains("✖"));
        assert!(!text.contains("iter-blocked"));
        let wiring = wiring_detail_lines(row, now)
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(wiring.contains("iter-blocked"));
    }

    #[test]
    fn wiring_keeps_iteration_ids() {
        let app = App::from_view(empty_view());
        let row = app.selected().unwrap();
        let now = UNIX_EPOCH
            + Duration::from_secs(parse_rfc3339_utc("2026-08-22T11:30:00Z").unwrap() as u64);
        let wiring = wiring_detail_lines(row, now)
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(wiring.contains("iter-active"));
        assert!(wiring.contains("iter-failed"));
        assert!(wiring.contains("iter-ok"));
    }

    #[test]
    fn iterations_are_newest_first_with_exit_code_only_glyphs() {
        let app = App::from_view(empty_view());
        let row = app.selected().unwrap();
        let now = UNIX_EPOCH
            + Duration::from_secs(parse_rfc3339_utc("2026-08-22T11:30:00Z").unwrap() as u64);
        let text = cockpit_plain(row, now);
        let active = text.find("●  10m ago  iteration in progress").unwrap();
        let failed = text
            .find("✖  40m ago  exited before reconsolidating a brief")
            .unwrap();
        let success = text.find("✓  1h ago  Queue drained").unwrap();
        let interrupted = text
            .find("?  2h ago  interrupted before producing a final brief")
            .unwrap();
        assert!(active < failed && failed < success && success < interrupted);
        assert!(!text.contains("iter-active"));
        assert!(!text.contains("iter-failed"));
        assert!(!text.contains("iter-ok"));
        let ok = &row.iterations[1];
        assert_eq!(ok.id, "iter-ok");
        assert_eq!(iteration_glyph(ok), "✓");
        assert_eq!(iteration_glyph(&row.iterations[0]), "✖");
        assert_eq!(iteration_glyph(&row.iterations[2]), "?");
    }

    #[test]
    fn row_keeps_latest_twenty_iterations_newest_first() {
        let mut view = empty_view();
        let operator = view.owned[0].get_mut("operator").unwrap();
        operator["active_iteration"] = Value::Null;
        operator["iterations"] = Value::Array(
            (0..25)
                .map(|i| {
                    serde_json::json!({
                        "id": format!("iter-{i:02}"),
                        "started_at": format!("2026-08-22T10:{i:02}:00Z"),
                        "finished_at": format!("2026-08-22T10:{i:02}:30Z"),
                        "exit_code": 0,
                        "reconsolidated": true,
                        "headline": format!("iteration {i}"),
                        "summary": null
                    })
                })
                .collect(),
        );
        let app = App::from_view(view);
        let row = app.selected().unwrap();
        assert_eq!(row.iterations.len(), 20);
        assert_eq!(row.iterations[0].id, "iter-24");
        assert_eq!(row.iterations[19].id, "iter-05");
    }

    #[test]
    fn cockpit_peeks_five_iterations_headline_only() {
        let mut view = empty_view();
        let operator = view.owned[0].get_mut("operator").unwrap();
        operator["active_iteration"] = Value::Null;
        operator["iterations"] = Value::Array(
            (0..8)
                .map(|i| {
                    serde_json::json!({
                        "id": format!("iter-{i:02}"),
                        "started_at": format!("2026-08-22T10:{i:02}:00Z"),
                        "finished_at": format!("2026-08-22T10:{i:02}:30Z"),
                        "exit_code": 0,
                        "reconsolidated": true,
                        "headline": format!("shipped {i}"),
                        "summary": format!("long body paragraph for iteration {i} that must not appear under the cockpit row")
                    })
                })
                .collect(),
        );
        let app = App::from_view(view);
        let row = app.selected().unwrap();
        assert_eq!(row.iterations.len(), 8);
        let now = UNIX_EPOCH
            + Duration::from_secs(parse_rfc3339_utc("2026-08-22T11:30:00Z").unwrap() as u64);
        let text = cockpit_plain(row, now);
        assert!(text.contains("✓  1h ago  shipped 7"));
        assert!(text.contains("✓  1h ago  shipped 3"));
        assert!(!text.contains("shipped 2"));
        assert!(!text.contains("long body paragraph"));
        let wiring = wiring_detail_lines(row, now)
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(wiring.contains("iter-07"));
        assert!(wiring.contains("iter-00"));
    }

    #[test]
    fn cockpit_iteration_line_does_not_use_summary() {
        let mut view = empty_view();
        let operator = view.owned[0].get_mut("operator").unwrap();
        operator["active_iteration"] = Value::Null;
        operator["iterations"] = serde_json::json!([{
            "id": "iter-summary-only",
            "started_at": "2026-08-22T10:00:00Z",
            "finished_at": "2026-08-22T10:05:00Z",
            "exit_code": 0,
            "reconsolidated": true,
            "headline": null,
            "summary": "paged vase with a long upgrade paragraph"
        }]);
        let app = App::from_view(view);
        let row = app.selected().unwrap();
        let now = UNIX_EPOCH
            + Duration::from_secs(parse_rfc3339_utc("2026-08-22T11:30:00Z").unwrap() as u64);
        let text = cockpit_plain(row, now);
        assert!(text.contains("✓  1h ago  completed"));
        assert!(!text.contains("paged vase"));
        assert!(!text.contains("iter-summary-only"));
    }

    #[test]
    fn detail_navigation_pages_and_jumps() {
        let mut app = App::from_view(empty_view());
        app.set_detail_extent(40, 10);
        assert!(handle_detail_navigation(&mut app, &KeyCode::PageDown));
        assert_eq!(app.detail_scroll, 10);
        handle_detail_navigation(&mut app, &KeyCode::PageDown);
        assert_eq!(app.detail_scroll, 20);
        handle_detail_navigation(&mut app, &KeyCode::PageUp);
        assert_eq!(app.detail_scroll, 10);
        handle_detail_navigation(&mut app, &KeyCode::End);
        assert_eq!(app.detail_scroll, 30);
        handle_detail_navigation(&mut app, &KeyCode::Home);
        assert_eq!(app.detail_scroll, 0);
        assert!(!handle_detail_navigation(&mut app, &KeyCode::Char('j')));
    }

    #[test]
    fn detail_scroll_resets_and_clamps() {
        let mut view = empty_view();
        view.owned.push(serde_json::json!({
            "unit": "managed-personal-other",
            "title": "Other",
            "health": "healthy",
            "operator": null,
            "operator_state": "missing"
        }));
        let mut app = App::from_view(view);
        app.set_detail_extent(40, 10);
        app.detail_end();
        assert_eq!(app.detail_scroll, 30);
        app.set_detail_extent(12, 10);
        assert_eq!(app.detail_scroll, 2);
        app.move_sel(1);
        assert_eq!(app.detail_scroll, 0);
        app.set_detail_extent(40, 10);
        app.detail_end();
        app.rebuild_rows();
        assert_eq!(app.detail_scroll, 30);

        app.set_detail_extent(40, 10);
        app.detail_end();
        app.toggle_wiring();
        assert_eq!(app.detail_scroll, 0);
        app.set_detail_extent(40, 10);
        app.detail_end();
        app.replace_view(empty_view());
        assert_eq!(app.detail_scroll, 30);
        app.set_detail_extent(18, 10);
        assert_eq!(app.detail_scroll, 8);
    }

    #[test]
    #[ignore = "manual cockpit capture"]
    fn capture_pr_maintainer_views() {
        let mut app = App::from_view(empty_view());
        let now = UNIX_EPOCH
            + Duration::from_secs(parse_rfc3339_utc("2026-08-22T11:30:00Z").unwrap() as u64);
        let row = app.selected().unwrap();
        let cockpit = cockpit_detail_lines(row, now)
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let wiring = wiring_detail_lines(row, now)
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        app.logs = vec![LogLine {
            text: "11:29:58  automation_report accepted".into(),
            alert: false,
        }];
        let backend = TestBackend::new(100, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let combined = buffer_text(&terminal);

        app.toggle_diagnostics();
        app.logs = vec![LogLine {
            text: "11:29:58  automation_report accepted".into(),
            alert: false,
        }];
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let logs = buffer_text(&terminal);

        println!("CAPTURE COCKPIT\n{cockpit}");
        println!("CAPTURE WIRING\n{wiring}");
        println!("CAPTURE COMBINED\n{combined}");
        println!("CAPTURE LOGS\n{logs}");
    }

    fn topology_fixture(text: Option<&str>) -> (PathBuf, App) {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "sdo-topo-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(".systemd-ops")).unwrap();
        if let Some(text) = text {
            std::fs::write(root.join(".systemd-ops").join("topology.txt"), text).unwrap();
        }
        let mut view = empty_view();
        view.root = root.clone();
        (root, App::from_view(view))
    }

    fn draw_app(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        buffer_text(&terminal)
    }

    #[test]
    fn topology_absent_is_calm() {
        let (_root, mut app) = topology_fixture(None);
        app.toggle_topology();
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("TOPOLOGY"));
        assert!(text.contains("not authored yet"));
        assert_eq!(app.detail_view, DetailView::Topology);
        assert!(!app.diagnostics_open);
        assert!(app.logs.is_empty());
    }

    #[test]
    fn topology_present_preserves_unicode_and_indent() {
        let diagram = "OMP\n│\n├─ release watch          deterministic\n│      │\n│      └─ new generation\n│             └────► HCOM → midi\n";
        let (_root, mut app) = topology_fixture(Some(diagram));
        app.toggle_topology();
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("TOPOLOGY"));
        assert!(text.contains("OMP"));
        assert!(text.contains("├─ release watch          deterministic"));
        assert!(text.contains("│      └─ new generation"));
        assert!(text.contains("└────► HCOM → midi"));
        assert!(!text.contains("not authored yet"));
    }

    #[test]
    fn topology_toggle_and_escape_return_to_cockpit() {
        let mut h = Harness::new();
        assert_eq!(h.app.detail_view, DetailView::Cockpit);
        h.key(KeyCode::Char('t'));
        assert_eq!(h.app.detail_view, DetailView::Topology);
        h.key(KeyCode::Char('t'));
        assert_eq!(h.app.detail_view, DetailView::Cockpit);
        h.key(KeyCode::Char('t'));
        h.key(KeyCode::Esc);
        assert_eq!(h.app.detail_view, DetailView::Cockpit);
        h.key(KeyCode::Char('d'));
        assert_eq!(h.app.detail_view, DetailView::Wiring);
        h.key(KeyCode::Char('t'));
        assert_eq!(h.app.detail_view, DetailView::Topology);
        h.key(KeyCode::Char('d'));
        assert_eq!(h.app.detail_view, DetailView::Wiring);
        h.key(KeyCode::Esc);
        assert_eq!(h.app.detail_view, DetailView::Cockpit);
    }

    #[test]
    fn topology_scrolls_with_detail_keys() {
        let mut lines = vec!["OMP".to_string()];
        for i in 0..40 {
            lines.push(format!("├─ item {i}"));
        }
        let (_root, mut app) = topology_fixture(Some(&format!("{}\n", lines.join("\n"))));
        app.toggle_topology();
        draw_app(&mut app, 80, 16);
        assert!(app.detail_max_scroll > 0);
        assert!(handle_detail_navigation(&mut app, &KeyCode::PageDown));
        assert!(app.detail_scroll > 0);
        handle_detail_navigation(&mut app, &KeyCode::End);
        assert_eq!(app.detail_scroll, app.detail_max_scroll);
        handle_detail_navigation(&mut app, &KeyCode::Home);
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn topology_resize_clamps_scroll() {
        let mut lines = vec!["OMP".to_string()];
        for i in 0..40 {
            lines.push(format!("├─ item {i}"));
        }
        let (_root, mut app) = topology_fixture(Some(&lines.join("\n")));
        app.toggle_topology();
        draw_app(&mut app, 80, 16);
        app.detail_end();
        let tall = app.detail_scroll;
        assert!(tall > 0);
        draw_app(&mut app, 40, 40);
        assert!(app.detail_scroll <= app.detail_max_scroll);
        draw_app(&mut app, 40, 12);
        assert!(app.detail_scroll <= app.detail_max_scroll);
    }

    #[test]
    fn topology_toggle_dispatches_no_backend() {
        let mut h = Harness::new();
        h.key(KeyCode::Char('t'));
        assert_eq!(h.dispatched.len(), 0);
        assert_eq!(h.app.detail_view, DetailView::Topology);
        h.key(KeyCode::Char('t'));
        assert_eq!(h.dispatched.len(), 0);
        h.key(KeyCode::Char('d'));
        h.key(KeyCode::Char('l'));
        assert_eq!(h.dispatched.len(), 0);
        assert_eq!(h.app.detail_view, DetailView::Wiring);
        assert!(h.app.diagnostics_open);
    }

    #[test]
    fn topology_draw_does_not_inspect_systemd() {
        let (_root, mut app) = topology_fixture(Some("LOCAL FILE\n"));
        app.toggle_topology();
        let text = draw_app(&mut app, 80, 24);
        assert!(text.contains("LOCAL FILE"));
        assert!(text.contains("TOPOLOGY"));
    }

    #[test]
    fn topology_keeps_existing_cockpit_wiring_logs() {
        let mut app = App::from_view(empty_view());
        app.toggle_topology();
        app.toggle_wiring();
        assert_eq!(app.detail_view, DetailView::Wiring);
        app.toggle_diagnostics();
        app.logs = vec![LogLine {
            text: "journal line".into(),
            alert: false,
        }];
        let text = draw_app(&mut app, 100, 34);
        assert!(
            text.contains("wiring")
                || text.contains("WIRING")
                || text.contains("identity")
                || text.contains("scope")
        );
        assert!(text.contains("journal line"));
        app.toggle_topology();
        assert_eq!(app.detail_view, DetailView::Topology);
        app.toggle_topology();
        assert_eq!(app.detail_view, DetailView::Cockpit);
        assert!(app.diagnostics_open);
    }

    fn two_ops_view() -> ScopeView {
        let mut view = empty_view();
        view.owned.push(serde_json::json!({
            "unit": "managed-personal-other",
            "title": "Other",
            "health": "healthy",
            "kind": "oneshot",
            "activation": "timer",
            "operator": Value::Null,
            "operator_state": "missing"
        }));
        view
    }

    struct Harness {
        app: App,
        gate: BackendCtl,
        dispatched: Vec<BackendJob>,
        quit: bool,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                app: App::from_view(two_ops_view()),
                gate: BackendCtl::default(),
                dispatched: Vec::new(),
                quit: false,
            }
        }

        fn dispatch(&mut self, job: BackendJob) {
            self.dispatched.push(job);
            self.app.refreshing = true;
            if self.app.inflight_since.is_none() {
                self.app.inflight_since = Some(Instant::now());
            }
        }

        fn request_refresh(&mut self) {
            if let Some(gen) = self.gate.request_refresh() {
                self.dispatch(BackendJob::Refresh {
                    gen,
                    scope_root: None,
                    cwd: None,
                });
            } else {
                self.app.refreshing = true;
            }
        }

        fn maybe_auto_refresh(&mut self) {
            if self.gate.inflight_refresh() || self.gate.busy {
                return;
            }
            if self.app.last_refresh.elapsed() >= REFRESH_EVERY {
                self.request_refresh();
            }
        }

        fn complete_refresh(&mut self, gen: u64, view: Result<ScopeView, String>) {
            let (apply, next) = self.gate.finish_refresh(gen);
            if apply {
                match view {
                    Ok(view) => self.app.replace_view(view),
                    Err(e) => {
                        self.app.last_error = Some(e);
                        self.app.last_refresh = Instant::now();
                    }
                }
            }
            if let Some(job) = next {
                self.dispatch(job);
            }
            self.app.refreshing = self.gate.inflight_refresh() || self.gate.busy;
            if !self.gate.busy {
                self.app.inflight_since = None;
            }
        }

        fn key(&mut self, code: KeyCode) -> bool {
            self.app.last_input = Instant::now();
            match code {
                KeyCode::Char('q') => {
                    self.quit = true;
                    return true;
                }
                KeyCode::Esc => {
                    let _ = self.app.return_cockpit();
                }
                KeyCode::Char('r') => self.request_refresh(),
                KeyCode::Char('d') => self.app.toggle_wiring(),
                KeyCode::Char('t') => self.app.toggle_topology(),
                KeyCode::Char('l') => self.app.toggle_diagnostics(),
                KeyCode::Down | KeyCode::Char('j') => self.app.move_sel(1),
                KeyCode::Up | KeyCode::Char('k') => self.app.move_sel(-1),
                other if handle_detail_navigation(&mut self.app, &other) => {}
                _ => {}
            }
            self.quit
        }

        fn selected_unit(&self) -> Option<String> {
            self.app.selected().map(|row| row.unit.clone())
        }
    }

    #[test]
    fn slow_refresh_does_not_block_navigation() {
        let mut h = Harness::new();
        let first = h.selected_unit();
        h.key(KeyCode::Char('r'));
        assert_eq!(h.dispatched.len(), 1);
        assert!(h.app.refreshing);
        h.key(KeyCode::Down);
        let moved = h.selected_unit();
        assert_ne!(moved, first);
        h.complete_refresh(1, Ok(two_ops_view()));
        assert_eq!(h.selected_unit(), moved);
        assert!(h.app.last_input <= Instant::now());
    }

    #[test]
    fn hung_refresh_still_quits() {
        let mut h = Harness::new();
        h.key(KeyCode::Char('r'));
        assert!(h.app.refreshing);
        assert!(h.key(KeyCode::Char('q')));
        assert!(h.quit);
        assert_eq!(h.dispatched.len(), 1);
    }

    #[test]
    fn repeated_refresh_does_not_queue_unbounded_jobs() {
        let mut h = Harness::new();
        h.request_refresh();
        h.request_refresh();
        h.request_refresh();
        h.request_refresh();
        h.request_refresh();
        assert_eq!(h.dispatched.len(), 1);
        h.complete_refresh(1, Ok(two_ops_view()));
        assert_eq!(h.dispatched.len(), 2);
        h.complete_refresh(2, Ok(two_ops_view()));
        assert_eq!(h.dispatched.len(), 2);
        assert!(!h.gate.busy);
    }

    #[test]
    fn stale_refresh_does_not_overwrite_newer_wanted_state() {
        let mut h = Harness::new();
        h.request_refresh();
        h.request_refresh();
        let mut poison = two_ops_view();
        poison.id = "poison".into();
        h.complete_refresh(1, Ok(poison));
        assert_ne!(h.app.view.id, "poison");
        assert_eq!(h.dispatched.len(), 2);
        match &h.dispatched[1] {
            BackendJob::Refresh { gen, .. } => assert_eq!(*gen, 2),
            _ => panic!("expected refresh"),
        }
        let mut fresh = two_ops_view();
        fresh.id = "fresh".into();
        h.complete_refresh(2, Ok(fresh));
        assert_eq!(h.app.view.id, "fresh");
    }

    #[test]
    fn auto_refresh_does_not_stale_inflight_or_storm() {
        let mut h = Harness::new();
        h.request_refresh();
        assert_eq!(h.dispatched.len(), 1);
        h.app.last_refresh = Instant::now() - REFRESH_EVERY;
        for _ in 0..20 {
            h.maybe_auto_refresh();
        }
        assert_eq!(h.dispatched.len(), 1);
        assert_eq!(h.gate.refresh_wanted, 1);
        let mut live = two_ops_view();
        live.id = "live".into();
        h.complete_refresh(1, Ok(live));
        assert_eq!(h.app.view.id, "live");
        assert_eq!(h.dispatched.len(), 1);
        assert!(!h.gate.busy);
        h.app.last_refresh = Instant::now() - REFRESH_EVERY;
        h.maybe_auto_refresh();
        assert_eq!(h.dispatched.len(), 2);
        h.maybe_auto_refresh();
        assert_eq!(h.dispatched.len(), 2);
    }

    #[test]
    fn failed_refresh_does_not_retry_every_poll_tick() {
        let mut h = Harness::new();
        h.request_refresh();
        h.complete_refresh(1, Err("systemctl timed out after 8000ms".into()));
        assert!(!h.gate.busy);
        for _ in 0..10 {
            h.maybe_auto_refresh();
        }
        assert_eq!(h.dispatched.len(), 1);
    }

    #[test]
    fn backend_failure_leaves_ui_usable() {
        let mut h = Harness::new();
        assert!(h.selected_unit().is_some());
        h.request_refresh();
        h.complete_refresh(1, Err("systemctl timed out after 8000ms".into()));
        assert!(h.app.last_error.as_deref().unwrap().contains("timed out"));
        let before = h.selected_unit();
        h.key(KeyCode::Down);
        assert!(h.selected_unit().is_some());
        assert!(h.app.last_error.as_deref().unwrap().contains("timed out"));
        assert_eq!(h.selected_unit().is_some(), before.is_some());
        assert!(!h.key(KeyCode::Char('x')));
        assert!(!h.quit);
    }

    #[test]
    fn loading_view_draw_does_not_touch_systemd() {
        let mut app = App::from_view(loading_view());
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.to_lowercase().contains("loading") || text.contains("LOADING"));
    }
}
