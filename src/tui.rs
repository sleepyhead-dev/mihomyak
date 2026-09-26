//! `mihomyak tui`: a small terminal UI over the mihomo API.
//!
//! Groups on the left, members with delays on the right. API calls run on
//! short-lived worker threads so delay tests never freeze the screen.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use crate::api::{Api, DELAY_TIMEOUT_MS, Snapshot};
use crate::config::Config;
use crate::store::Store;
use crate::subscription::ProviderInfo;
use crate::util::{fmt_bytes, fmt_date, now_unix, sanitize};

const REFRESH_EVERY: Duration = Duration::from_secs(3);

enum Msg {
    Snapshot(Result<Snapshot, String>),
    Mode(String),
    Delays(HashMap<String, u32>),
    Status(String),
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Pane {
    Groups,
    Members,
}

struct App {
    api: Arc<Api>,
    store: Store,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    snapshot: Snapshot,
    mode: String,
    version: String,
    info: Option<ProviderInfo>,
    pane: Pane,
    groups: ListState,
    members: ListState,
    status: String,
    last_refresh: Instant,
    refreshing: bool,
    quit: bool,
}

pub fn run(config: &Config) -> Result<()> {
    let api = Api::from_config(config)?;
    let version = api
        .version()
        .map_err(|e| anyhow::anyhow!("mihomo API unreachable: {e:#}"))?;
    let store = Store::open(&config.data_dir)?;
    let (tx, rx) = mpsc::channel();
    let mut app = App {
        api: Arc::new(api),
        info: store
            .load_meta()
            .map(|m| ProviderInfo::from_headers(&m.headers)),
        store,
        tx,
        rx,
        snapshot: Snapshot::default(),
        mode: String::new(),
        version,
        pane: Pane::Groups,
        groups: ListState::default().with_selected(Some(0)),
        members: ListState::default().with_selected(Some(0)),
        status: "↑↓ move  ←→ pane  enter select  t test  m mode  u update  r refresh  q quit"
            .into(),
        last_refresh: Instant::now() - REFRESH_EVERY,
        refreshing: false,
        quit: false,
    };
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

impl App {
    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            if !self.refreshing && self.last_refresh.elapsed() >= REFRESH_EVERY {
                self.refresh();
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.handle(msg);
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(200))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.key(key.code, key.modifiers);
            }
        }
        Ok(())
    }

    fn spawn(&self, job: impl FnOnce(&Api) -> Msg + Send + 'static) {
        let (api, tx) = (self.api.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(job(&api));
        });
    }

    fn refresh(&mut self) {
        self.refreshing = true;
        self.last_refresh = Instant::now();
        self.spawn(|api| Msg::Snapshot(api.snapshot().map_err(|e| format!("{e:#}"))));
        self.spawn(|api| Msg::Mode(api.mode().unwrap_or_default()));
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Snapshot(Ok(s)) => {
                self.refreshing = false;
                self.snapshot = s;
                clamp(&mut self.groups, self.snapshot.groups.len());
                let members = self.group().map_or(0, |g| g.members.len());
                clamp(&mut self.members, members);
            }
            Msg::Snapshot(Err(e)) => {
                self.refreshing = false;
                self.status = format!("API error: {e}");
            }
            Msg::Mode(mode) => self.mode = mode,
            Msg::Delays(delays) => {
                let ok = delays.values().filter(|&&d| d > 0).count();
                self.status = format!("delay test done: {ok} of {} reachable", delays.len());
                self.snapshot.delays.extend(delays);
            }
            Msg::Status(s) => {
                self.status = s;
                self.refresh();
            }
        }
    }

    fn group(&self) -> Option<&crate::api::Group> {
        self.groups
            .selected()
            .and_then(|i| self.snapshot.groups.get(i))
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Left | KeyCode::Char('h') => self.pane = Pane::Groups,
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => self.pane = Pane::Members,
            KeyCode::Enter => match self.pane {
                Pane::Groups => self.pane = Pane::Members,
                Pane::Members => self.select(),
            },
            KeyCode::Char('t') => self.test(),
            KeyCode::Char('m') => self.cycle_mode(),
            KeyCode::Char('u') => self.update(),
            KeyCode::Char('r') => self.refresh(),
            _ => {}
        }
    }

    fn move_by(&mut self, delta: isize) {
        let (state, len) = match self.pane {
            Pane::Groups => (&mut self.groups, self.snapshot.groups.len()),
            Pane::Members => {
                let len = self
                    .groups
                    .selected()
                    .and_then(|i| self.snapshot.groups.get(i))
                    .map_or(0, |g| g.members.len());
                (&mut self.members, len)
            }
        };
        if len == 0 {
            return;
        }
        let current = state.selected().unwrap_or(0) as isize;
        state.select(Some((current + delta).rem_euclid(len as isize) as usize));
        if self.pane == Pane::Groups {
            // Start the member list at the group's current choice.
            let now_idx = self
                .group()
                .and_then(|g| g.members.iter().position(|m| Some(m) == g.now.as_ref()));
            self.members.select(Some(now_idx.unwrap_or(0)));
        }
    }

    fn select(&mut self) {
        let Some(group) = self.group().cloned() else {
            return;
        };
        if !group.selectable() {
            self.status = format!(
                "{} is {}: selection is automatic",
                sanitize(&group.name),
                group.kind
            );
            return;
        }
        let Some(proxy) = self
            .members
            .selected()
            .and_then(|i| group.members.get(i))
            .cloned()
        else {
            return;
        };
        self.spawn(move |api| {
            Msg::Status(match api.select(&group.name, &proxy) {
                Ok(()) => format!("{} → {}", sanitize(&group.name), sanitize(&proxy)),
                Err(e) => format!("select failed: {e:#}"),
            })
        });
    }

    fn test(&mut self) {
        let Some((group, members)) = self.group().map(|g| (g.name.clone(), g.members.clone()))
        else {
            return;
        };
        self.status = format!("testing {}…", sanitize(&group));
        let url = crate::profile::HEALTH_CHECK_URL;
        self.spawn(
            move |api| match api.group_delay(&group, url, DELAY_TIMEOUT_MS) {
                // Members missing from the answer failed: show them as timeouts (0).
                Ok(mut delays) => {
                    for member in members {
                        delays.entry(member).or_insert(0);
                    }
                    Msg::Delays(delays)
                }
                Err(e) => Msg::Status(format!("delay test failed: {e:#}")),
            },
        );
    }

    fn cycle_mode(&mut self) {
        let next = match self.mode.as_str() {
            "rule" => "global",
            "global" => "direct",
            _ => "rule",
        };
        let store = self.store.clone();
        self.spawn(move |api| {
            Msg::Status(
                match api.set_mode(next).and_then(|()| store.set_mode(next)) {
                    Ok(()) => format!("mode: {next}"),
                    Err(e) => format!("mode change failed: {e:#}"),
                },
            )
        });
    }

    fn update(&mut self) {
        self.status = match self.store.signal_supervisor(libc::SIGHUP) {
            Ok(Some(_)) => "subscription update requested (see supervisor logs)".into(),
            Ok(None) => "supervisor not running: use `mihomyak update`".into(),
            Err(e) => format!("{e:#}"),
        };
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        frame.render_widget(self.header(), header);

        let [left, right] =
            Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)])
                .areas(body);
        let focus = |pane| {
            if self.pane == pane {
                Style::new().fg(Color::Cyan)
            } else {
                Style::new().fg(Color::DarkGray)
            }
        };
        let highlight = Style::new().add_modifier(Modifier::REVERSED);

        let groups: Vec<ListItem> = self
            .snapshot
            .groups
            .iter()
            .map(|g| {
                ListItem::new(Line::from(vec![
                    Span::raw(sanitize(&g.name)),
                    Span::raw(format!("  → {}", sanitize(g.now.as_deref().unwrap_or("-"))))
                        .dark_gray(),
                ]))
            })
            .collect();
        frame.render_stateful_widget(
            List::new(groups)
                .block(
                    Block::bordered()
                        .title(" Groups ")
                        .border_style(focus(Pane::Groups)),
                )
                .highlight_style(highlight),
            left,
            &mut self.groups,
        );

        let (title, members) = match self.group() {
            Some(g) => (
                format!(" {} [{}] ", sanitize(&g.name), g.kind),
                g.members
                    .iter()
                    .map(|m| {
                        let current = g.now.as_ref() == Some(m);
                        let kind = self.snapshot.kinds.get(m).cloned().unwrap_or_default();
                        ListItem::new(Line::from(vec![
                            Span::raw(if current { "● " } else { "  " }).green(),
                            Span::raw(sanitize(m)),
                            Span::raw(format!("  {kind}  ")).dark_gray(),
                            delay_span(self.snapshot.delays.get(m)),
                        ]))
                    })
                    .collect(),
            ),
            None => (" Proxies ".into(), Vec::new()),
        };
        frame.render_stateful_widget(
            List::new(members)
                .block(
                    Block::bordered()
                        .title(title)
                        .border_style(focus(Pane::Members)),
                )
                .highlight_style(highlight),
            right,
            &mut self.members,
        );
        frame.render_widget(Paragraph::new(self.status.clone()).dark_gray(), footer);
    }

    fn header(&self) -> Paragraph<'_> {
        let title = self
            .info
            .as_ref()
            .and_then(|i| i.title.as_deref())
            .map_or_else(|| "subscription".into(), sanitize);
        let mut usage = Vec::new();
        if let Some(u) = self.info.as_ref().and_then(|i| i.usage.as_ref()) {
            usage.push(format!(
                "traffic {} / {}",
                fmt_bytes(u.used()),
                u.total_display()
            ));
            if u.expire > 0 {
                usage.push(format!(
                    "expires {} ({}d)",
                    fmt_date(u.expire),
                    u.days_left(now_unix())
                ));
            }
        }
        Paragraph::new(vec![
            Line::from(vec![
                Span::raw(title).bold(),
                Span::raw(format!("   mode: {}   {}", self.mode, self.version)).dark_gray(),
            ]),
            Line::from(usage.join("  ·  ")),
        ])
        .block(Block::bordered().title(" mihomyak "))
    }
}

fn delay_span(delay: Option<&u32>) -> Span<'static> {
    match delay {
        Some(&d) if d > 0 && d < 300 => Span::raw(format!("{d} ms")).green(),
        Some(&d) if d > 0 && d < 800 => Span::raw(format!("{d} ms")).yellow(),
        Some(&d) if d > 0 => Span::raw(format!("{d} ms")).red(),
        Some(_) => Span::raw("timeout").red(),
        None => Span::raw("-").dark_gray(),
    }
}

fn clamp(state: &mut ListState, len: usize) {
    match (state.selected(), len) {
        (_, 0) => state.select(None),
        (None, _) => state.select(Some(0)),
        (Some(i), n) if i >= n => state.select(Some(n - 1)),
        _ => {}
    }
}
