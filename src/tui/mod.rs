use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::Show;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::core::state::HealthState;
use crate::doctor;
use crate::runtime::detached::{DetachedRuntime, DetachedServiceStatus};
use crate::runtime::logs::{tail_history, FileFollower, Redactor};

mod ui;

const EVENT_TICK: Duration = Duration::from_millis(250);
const RUNTIME_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_LOG_LINES: usize = 500;
const MAX_LOG_VIEW_BYTES: usize = 4 * 1024 * 1024;
const DETAIL_LINE_COUNT: u16 = 16;
const DETAIL_HORIZONTAL_LIMIT: u16 = 512;

#[derive(Debug, PartialEq)]
enum Mode {
    Normal,
    TemplateSelect,
    Logs,
    Details,
    Doctor,
    QuitConfirm,
    Help,
}

pub struct App {
    pub runtime: Arc<DetachedRuntime>,
    pub statuses: HashMap<String, DetachedServiceStatus>,
    pub services: Vec<String>,
    pub selected: usize,
    pub template: Option<String>,
    pub log_lines: VecDeque<String>,
    log_bytes: usize,
    pub log_search: String,
    pub log_searching: bool,
    pub details_scroll: u16,
    pub details_horizontal: u16,
    mode: Mode,
    template_cursor: usize,
    doctor_results: Vec<doctor::DoctorCheck>,
    pub doctor_scroll: u16,
    status_line: String,
    should_quit: bool,
    health_due: HashMap<String, Instant>,
    stdout_follower: Option<FileFollower>,
    stderr_follower: Option<FileFollower>,
    log_service: Option<String>,
    redactor: Redactor,
    active_poll: Option<u64>,
    poll_task: Option<tokio::task::JoinHandle<()>>,
    poll_guard: Arc<tokio::sync::Mutex<()>>,
    next_poll_id: u64,
    runtime_due: Instant,
    health_in_flight: HashSet<String>,
    action_in_flight: bool,
    doctor_in_flight: bool,
}

impl App {
    fn new(runtime: Arc<DetachedRuntime>, template: Option<String>) -> Self {
        let services = template
            .as_deref()
            .and_then(|name| crate::core::graph::services_for_template(runtime.config(), name).ok())
            .unwrap_or_default();
        let redactor = Redactor::new(&runtime.config().logs.redact_patterns)
            .expect("validated redaction patterns");
        App {
            runtime,
            statuses: HashMap::new(),
            services,
            selected: 0,
            template,
            log_lines: VecDeque::with_capacity(MAX_LOG_LINES),
            log_bytes: 0,
            log_search: String::new(),
            log_searching: false,
            details_scroll: 0,
            details_horizontal: 0,
            mode: Mode::Normal,
            template_cursor: 0,
            doctor_results: Vec::new(),
            doctor_scroll: 0,
            status_line: String::new(),
            should_quit: false,
            health_due: HashMap::new(),
            stdout_follower: None,
            stderr_follower: None,
            log_service: None,
            redactor,
            active_poll: None,
            poll_task: None,
            poll_guard: Arc::new(tokio::sync::Mutex::new(())),
            next_poll_id: 0,
            runtime_due: Instant::now(),
            health_in_flight: HashSet::new(),
            action_in_flight: false,
            doctor_in_flight: false,
        }
    }

    fn selected_name(&self) -> Option<String> {
        self.services.get(self.selected).cloned()
    }

    pub(crate) fn template_cursor(&self) -> usize {
        self.template_cursor
    }

    fn next(&mut self) {
        if !self.services.is_empty() {
            self.selected = (self.selected + 1) % self.services.len();
        }
    }

    fn prev(&mut self) {
        if !self.services.is_empty() {
            self.selected = if self.selected == 0 {
                self.services.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    fn invalidate_poll(&mut self) {
        self.next_poll_id = self.next_poll_id.wrapping_add(1);
        self.active_poll = None;
        if let Some(task) = self.poll_task.take() {
            task.abort();
        }
        self.runtime_due = Instant::now();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(task) = self.poll_task.take() {
            task.abort();
        }
    }
}

struct HealthUpdate {
    name: String,
    pid: Option<u32>,
    result: Result<(HealthState, String, u64), String>,
}

struct PollMessage {
    id: u64,
    result: Result<Vec<DetachedServiceStatus>, String>,
}

enum MonitorMessage {
    Poll(PollMessage),
    Health(HealthUpdate),
}

struct ActionMessage {
    result: Result<String, String>,
    quit_after_success: bool,
}

struct DoctorMessage {
    result: Result<Vec<doctor::DoctorCheck>, String>,
}

/// Launch the interactive monitor. Quitting the TUI never stops services.
pub async fn run(runtime: Arc<DetachedRuntime>, template: Option<String>) -> Result<()> {
    let mut session = TerminalSession::enter()?;
    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, runtime, template).await;
    drop(terminal);
    let restored = session.restore();
    match (result, restored) {
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    }
}

struct TerminalSession {
    raw: bool,
    alternate: bool,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut session = Self {
            raw: true,
            alternate: false,
        };
        let mut stdout = std::io::stdout();
        session.alternate = true;
        execute!(stdout, EnterAlternateScreen)?;
        Ok(session)
    }

    fn restore(&mut self) -> Result<()> {
        let mut failure = None;
        if self.raw {
            match disable_raw_mode() {
                Ok(()) => self.raw = false,
                Err(error) => failure = Some(anyhow::Error::from(error)),
            }
        }
        if self.alternate {
            let mut stdout = std::io::stdout();
            match execute!(stdout, LeaveAlternateScreen, Show) {
                Ok(()) => self.alternate = false,
                Err(error) => {
                    failure.get_or_insert_with(|| anyhow::Error::from(error));
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    runtime: Arc<DetachedRuntime>,
    template: Option<String>,
) -> Result<()> {
    let mut app = App::new(runtime, template);
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(EVENT_TICK);
    let (monitor_tx, mut monitor_rx) = mpsc::unbounded_channel();
    let (action_tx, mut action_rx) = mpsc::unbounded_channel();
    let (doctor_tx, mut doctor_rx) = mpsc::unbounded_channel();
    schedule_poll(&mut app, &monitor_tx);

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            _ = ticker.tick() => {
                refresh_log_followers(&mut app);
                schedule_poll(&mut app, &monitor_tx);
                schedule_health_checks(&mut app, &monitor_tx);
            }
            Some(message) = monitor_rx.recv() => match message {
                MonitorMessage::Poll(message) => apply_poll(&mut app, message, &monitor_tx),
                MonitorMessage::Health(message) => apply_health(&mut app, message),
            },
            Some(message) = action_rx.recv() => apply_action(&mut app, message, &monitor_tx),
            Some(message) = doctor_rx.recv() => apply_doctor(&mut app, message),
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(&mut app, key.code, &action_tx, &doctor_tx);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                    None => anyhow::bail!("terminal event stream closed"),
                }
            }
        }
    }
}

fn schedule_poll(app: &mut App, tx: &mpsc::UnboundedSender<MonitorMessage>) {
    if app.action_in_flight || app.active_poll.is_some() || Instant::now() < app.runtime_due {
        return;
    }
    let Some(template) = app.template.clone() else {
        return;
    };

    app.next_poll_id = app.next_poll_id.wrapping_add(1);
    let id = app.next_poll_id;
    app.active_poll = Some(id);
    app.runtime_due = Instant::now() + RUNTIME_POLL_INTERVAL;
    let runtime = app.runtime.clone();
    let poll_guard = app.poll_guard.clone();
    let tx = tx.clone();

    app.poll_task = Some(tokio::spawn(async move {
        // Aborting a Tokio task is cooperative. This guard also serializes the
        // synchronous PID/port portion of an already-running pass, so an
        // invalidated poll can never overlap its replacement.
        let _guard = poll_guard.lock_owned().await;
        let result = runtime
            .monitor_template(&template)
            .await
            .map_err(|error| error.to_string());
        let _ = tx.send(MonitorMessage::Poll(PollMessage { id, result }));
    }));
}

fn apply_poll(app: &mut App, message: PollMessage, tx: &mpsc::UnboundedSender<MonitorMessage>) {
    if app.active_poll != Some(message.id) {
        return;
    }
    app.active_poll = None;
    app.poll_task.take();

    let mut statuses = match message.result {
        Ok(statuses) => statuses,
        Err(error) => {
            app.status_line = format!("monitor error: {error}");
            return;
        }
    };
    let now = Instant::now();

    for status in &mut statuses {
        let healthcheck = app
            .runtime
            .config()
            .services
            .get(&status.name)
            .and_then(|service| service.healthcheck.as_ref());
        if !status.process.is_running() {
            status.health = HealthState::Unchecked;
            status.health_detail = None;
            status.health_duration_ms = None;
            app.health_due.insert(status.name.clone(), now);
            continue;
        }
        let Some(_healthcheck) = healthcheck else {
            status.health = HealthState::Unchecked;
            status.health_detail = None;
            status.health_duration_ms = None;
            app.health_due.insert(status.name.clone(), now);
            continue;
        };
        if let Some(previous) = app
            .statuses
            .get(&status.name)
            .filter(|previous| previous.pid == status.pid)
        {
            status.health = previous.health;
            status.health_detail.clone_from(&previous.health_detail);
            status.health_duration_ms = previous.health_duration_ms;
        } else {
            status.health = HealthState::Checking;
        }
    }

    app.services = statuses.iter().map(|status| status.name.clone()).collect();
    app.selected = app.selected.min(app.services.len().saturating_sub(1));
    app.statuses = statuses
        .into_iter()
        .map(|status| (status.name.clone(), status))
        .collect();
    schedule_health_checks(app, tx);
}

fn schedule_health_checks(app: &mut App, tx: &mpsc::UnboundedSender<MonitorMessage>) {
    let now = Instant::now();
    let due = app
        .statuses
        .values()
        .filter(|status| {
            status.process.is_running()
                && app
                    .runtime
                    .config()
                    .services
                    .get(&status.name)
                    .is_some_and(|service| service.healthcheck.is_some())
                && app.health_due.get(&status.name).copied().unwrap_or(now) <= now
                && !app.health_in_flight.contains(&status.name)
        })
        .map(|status| (status.name.clone(), status.pid))
        .collect::<Vec<_>>();

    for (name, pid) in due {
        app.health_in_flight.insert(name.clone());
        let runtime = app.runtime.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = runtime
                .check_service_health(&name)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(MonitorMessage::Health(HealthUpdate { name, pid, result }));
        });
    }
}

fn apply_health(app: &mut App, update: HealthUpdate) {
    app.health_in_flight.remove(&update.name);
    let Some(status) = app.statuses.get_mut(&update.name) else {
        app.health_due.insert(update.name, Instant::now());
        return;
    };
    if !status.process.is_running() || status.pid != update.pid {
        app.health_due.insert(update.name, Instant::now());
        return;
    }
    let interval = app
        .runtime
        .config()
        .services
        .get(&update.name)
        .and_then(|service| service.healthcheck.as_ref())
        .map(crate::runtime::health::interval)
        .unwrap_or(RUNTIME_POLL_INTERVAL);
    app.health_due
        .insert(update.name.clone(), Instant::now() + interval);
    match update.result {
        Ok((state, detail, duration_ms)) => {
            status.health = state;
            status.health_detail = Some(detail);
            status.health_duration_ms = Some(duration_ms);
        }
        Err(error) => {
            status.health = HealthState::Unhealthy;
            status.health_detail = Some(error);
            status.health_duration_ms = None;
        }
    }
}

fn apply_action(
    app: &mut App,
    message: ActionMessage,
    monitor_tx: &mpsc::UnboundedSender<MonitorMessage>,
) {
    app.action_in_flight = false;
    let succeeded = message.result.is_ok();
    app.status_line = match message.result {
        Ok(detail) => detail,
        Err(error) => format!("action failed: {error}"),
    };
    if succeeded && message.quit_after_success {
        app.should_quit = true;
        return;
    }
    app.invalidate_poll();
    schedule_poll(app, monitor_tx);
}

fn apply_doctor(app: &mut App, message: DoctorMessage) {
    app.doctor_in_flight = false;
    match message.result {
        Ok(results) => {
            let failures = results.iter().filter(|result| !result.ok).count();
            app.doctor_results = results;
            app.status_line = if failures == 0 {
                "doctor completed: all checks passed".to_string()
            } else {
                format!("doctor completed: {failures} check(s) failed")
            };
        }
        Err(error) => app.status_line = format!("doctor failed: {error}"),
    }
}

fn handle_key(
    app: &mut App,
    code: KeyCode,
    action_tx: &mpsc::UnboundedSender<ActionMessage>,
    doctor_tx: &mpsc::UnboundedSender<DoctorMessage>,
) {
    match app.mode {
        Mode::TemplateSelect => {
            let mut templates: Vec<String> =
                app.runtime.config().templates.keys().cloned().collect();
            templates.sort();
            match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.template_cursor == 0 {
                        app.template_cursor = templates.len().saturating_sub(1);
                    } else {
                        app.template_cursor -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !templates.is_empty() {
                        app.template_cursor = (app.template_cursor + 1) % templates.len();
                    }
                }
                KeyCode::Enter => {
                    if let Some(template) = templates.get(app.template_cursor) {
                        app.template = Some(template.clone());
                        app.services = crate::core::graph::services_for_template(
                            app.runtime.config(),
                            template,
                        )
                        .unwrap_or_default();
                        app.selected = 0;
                        app.status_line = format!("monitoring template '{template}'");
                        app.invalidate_poll();
                    } else {
                        app.status_line = "no templates are configured".to_string();
                    }
                    app.mode = Mode::Normal;
                }
                KeyCode::Esc => app.mode = Mode::Normal,
                _ => {}
            }
        }
        Mode::Logs => {
            if app.log_searching {
                match code {
                    KeyCode::Enter => app.log_searching = false,
                    KeyCode::Esc => {
                        app.log_searching = false;
                        app.log_search.clear();
                    }
                    KeyCode::Backspace => {
                        app.log_search.pop();
                    }
                    KeyCode::Char(character) => app.log_search.push(character),
                    _ => {}
                }
                return;
            }
            match code {
                KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::Normal,
                KeyCode::Char('c') => clear_log_view(app),
                KeyCode::Char('/') => {
                    app.log_search.clear();
                    app.log_searching = true;
                }
                _ => {}
            }
        }
        Mode::Details => match code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.details_scroll = app.details_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.details_scroll = app
                    .details_scroll
                    .saturating_add(1)
                    .min(DETAIL_LINE_COUNT.saturating_sub(1));
            }
            KeyCode::PageUp => {
                app.details_scroll = app.details_scroll.saturating_sub(8);
            }
            KeyCode::PageDown => {
                app.details_scroll = app
                    .details_scroll
                    .saturating_add(8)
                    .min(DETAIL_LINE_COUNT.saturating_sub(1));
            }
            KeyCode::Left | KeyCode::Char('h') => {
                app.details_horizontal = app.details_horizontal.saturating_sub(4);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                app.details_horizontal = app
                    .details_horizontal
                    .saturating_add(4)
                    .min(DETAIL_HORIZONTAL_LIMIT);
            }
            KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::Normal,
            _ => {}
        },
        Mode::Doctor => match code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.doctor_scroll = app.doctor_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.doctor_scroll = app
                    .doctor_scroll
                    .saturating_add(1)
                    .min(app.doctor_results.len().saturating_sub(1) as u16);
            }
            KeyCode::PageUp => {
                app.doctor_scroll = app.doctor_scroll.saturating_sub(8);
            }
            KeyCode::PageDown => {
                app.doctor_scroll = app
                    .doctor_scroll
                    .saturating_add(8)
                    .min(app.doctor_results.len().saturating_sub(1) as u16);
            }
            KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::Normal,
            _ => {}
        },
        Mode::Help => {
            if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                app.mode = Mode::Normal;
            }
        }
        Mode::QuitConfirm => match code {
            _ if app.action_in_flight => {
                app.status_line = "wait for the stop action to finish".to_string();
            }
            KeyCode::Char('l') => app.should_quit = true,
            KeyCode::Char('s') => {
                if app.action_in_flight {
                    app.status_line = "another action is still running".to_string();
                    return;
                }
                let Some(template) = app.template.clone() else {
                    app.status_line = "no template selected; nothing was stopped".to_string();
                    app.mode = Mode::Normal;
                    return;
                };
                let runtime = app.runtime.clone();
                start_action_with_quit(
                    app,
                    action_tx,
                    format!("stopping template '{template}' before quit..."),
                    async move {
                        let report = runtime
                            .stop_template(&template, Duration::from_secs(10))
                            .await
                            .map_err(|error| error.to_string())?;
                        if report.succeeded() {
                            Ok(format!("template '{template}' stopped"))
                        } else {
                            Err(report.failures[0].detail.clone())
                        }
                    },
                );
            }
            KeyCode::Esc => app.mode = Mode::Normal,
            _ => {}
        },
        Mode::Normal => match code {
            KeyCode::Char('q') => {
                if app.action_in_flight {
                    app.status_line =
                        "wait for the explicit start/stop/restart action to finish".to_string();
                } else if app.doctor_in_flight {
                    app.status_line = "wait for doctor to finish before quitting".to_string();
                } else {
                    app.mode = Mode::QuitConfirm;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => app.prev(),
            KeyCode::Down | KeyCode::Char('j') => app.next(),
            KeyCode::Char(' ') => {
                if let Some(name) = app.selected_name() {
                    if app.action_in_flight {
                        app.status_line = "another action is still running".to_string();
                        return;
                    }
                    let running = app
                        .statuses
                        .get(&name)
                        .map(|status| status.process.is_running())
                        .unwrap_or(false);
                    let runtime = app.runtime.clone();
                    if running {
                        let action_name = name.clone();
                        start_action(
                            app,
                            action_tx,
                            format!("stopping '{name}'..."),
                            async move {
                                let report = runtime
                                    .stop_services(
                                        std::slice::from_ref(&action_name),
                                        Duration::from_secs(10),
                                    )
                                    .await
                                    .map_err(|error| error.to_string())?;
                                if !report.succeeded() {
                                    Err(report.failures[0].detail.clone())
                                } else if report.stopped.contains(&action_name) {
                                    Ok(format!("service '{action_name}' stopped"))
                                } else {
                                    Ok(format!("service '{action_name}' was already stopped"))
                                }
                            },
                        );
                    } else {
                        let action_name = name.clone();
                        start_action(
                            app,
                            action_tx,
                            format!("starting '{name}'..."),
                            async move {
                                let report = runtime
                                    .start_services(std::slice::from_ref(&action_name))
                                    .await
                                    .map_err(|error| error.to_string())?;
                                if report.started.contains(&action_name) {
                                    Ok(format!("service '{action_name}' started"))
                                } else {
                                    Ok(format!("service '{action_name}' was already running"))
                                }
                            },
                        );
                    }
                } else {
                    app.status_line = "this template contains no services".to_string();
                }
            }
            KeyCode::Char('r') => {
                if let Some(name) = app.selected_name() {
                    if app.action_in_flight {
                        app.status_line = "another action is still running".to_string();
                        return;
                    }
                    let runtime = app.runtime.clone();
                    let action_name = name.clone();
                    start_action(
                        app,
                        action_tx,
                        format!("restarting '{name}'..."),
                        async move {
                            let report = runtime
                                .restart_services(
                                    std::slice::from_ref(&action_name),
                                    Duration::from_secs(10),
                                )
                                .await
                                .map_err(|error| error.to_string())?;
                            if report.stop.succeeded() {
                                Ok(format!("service '{action_name}' restarted"))
                            } else {
                                Err(report.stop.failures[0].detail.clone())
                            }
                        },
                    );
                } else {
                    app.status_line = "this template contains no services".to_string();
                }
            }
            KeyCode::Enter => {
                if app
                    .selected_name()
                    .is_some_and(|name| app.statuses.contains_key(&name))
                {
                    app.details_scroll = 0;
                    app.details_horizontal = 0;
                    app.mode = Mode::Details;
                } else if app.selected_name().is_some() {
                    app.status_line = "service status is still loading".to_string();
                } else {
                    app.status_line = "this template contains no services".to_string();
                }
            }
            KeyCode::Char('l') => {
                if app.selected_name().is_some() {
                    open_logs(app);
                } else {
                    app.status_line = "this template contains no services".to_string();
                }
            }
            KeyCode::Char('p') => {
                app.mode = Mode::TemplateSelect;
                app.template_cursor = 0;
            }
            KeyCode::Char('d') => {
                if app.action_in_flight {
                    app.status_line =
                        "wait for the current action before running doctor".to_string();
                    return;
                }
                if app.doctor_in_flight {
                    app.status_line = "doctor is already running".to_string();
                    return;
                }
                app.mode = Mode::Doctor;
                app.doctor_scroll = 0;
                app.doctor_in_flight = true;
                app.status_line = "doctor running...".to_string();
                let runtime = app.runtime.clone();
                let tx = doctor_tx.clone();
                tokio::spawn(async move {
                    let result =
                        tokio::task::spawn_blocking(move || doctor::run_with_runtime(&runtime))
                            .await
                            .map_err(|error| error.to_string());
                    let _ = tx.send(DoctorMessage { result });
                });
            }
            KeyCode::Char('o') => {
                if let Some(name) = app.selected_name() {
                    if let Some(url) = app
                        .runtime
                        .config()
                        .services
                        .get(&name)
                        .and_then(|service| service.url.clone())
                    {
                        start_action(app, action_tx, format!("opening {url}..."), async move {
                            tokio::task::spawn_blocking(move || open::that(url))
                                .await
                                .map_err(|error| error.to_string())?
                                .map(|_| "URL opened".to_string())
                                .map_err(|error| error.to_string())
                        });
                    } else {
                        app.status_line = format!("service '{name}' has no URL configured");
                    }
                } else {
                    app.status_line = "this template contains no services".to_string();
                }
            }
            KeyCode::Char('?') => app.mode = Mode::Help,
            _ => {}
        },
    }
}

fn start_action<F>(
    app: &mut App,
    tx: &mpsc::UnboundedSender<ActionMessage>,
    pending: String,
    action: F,
) where
    F: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    start_action_inner(app, tx, pending, false, action);
}

fn start_action_with_quit<F>(
    app: &mut App,
    tx: &mpsc::UnboundedSender<ActionMessage>,
    pending: String,
    action: F,
) where
    F: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    start_action_inner(app, tx, pending, true, action);
}

fn start_action_inner<F>(
    app: &mut App,
    tx: &mpsc::UnboundedSender<ActionMessage>,
    pending: String,
    quit_after_success: bool,
    action: F,
) where
    F: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    if app.action_in_flight {
        app.status_line = "another action is still running".to_string();
        return;
    }
    if app.doctor_in_flight {
        app.status_line = "wait for doctor to finish before starting an action".to_string();
        return;
    }
    app.action_in_flight = true;
    app.status_line = pending;
    app.invalidate_poll();
    let tx = tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(ActionMessage {
            result: action.await,
            quit_after_success,
        });
    });
}

fn open_logs(app: &mut App) {
    let Some(name) = app.selected_name() else {
        return;
    };
    let Ok((stdout_path, stderr_path)) = app.runtime.log_paths(&name) else {
        app.status_line = format!("could not locate logs for '{name}'");
        return;
    };
    clear_log_view(app);
    app.log_search.clear();
    app.log_searching = false;
    let rotated_files = app.runtime.config().logs.rotated_files;
    let max_line_bytes = app.runtime.config().logs.max_line_bytes;
    let mut stdout_follower = FileFollower::from_end_with_limit(&stdout_path, max_line_bytes)
        .ok()
        .flatten();
    let mut stderr_follower = FileFollower::from_end_with_limit(&stderr_path, max_line_bytes)
        .ok()
        .flatten();
    let stdout_tail = match stdout_follower.as_mut() {
        Some(follower) => follower.initial_tail(200, rotated_files),
        None => tail_history(&stdout_path, 200, rotated_files),
    };
    match stdout_tail {
        Ok(lines) => append_log_lines(app, "stdout", lines),
        Err(error) => app.status_line = error.to_string(),
    }
    let stderr_tail = match stderr_follower.as_mut() {
        Some(follower) => follower.initial_tail(200, rotated_files),
        None => tail_history(&stderr_path, 200, rotated_files),
    };
    match stderr_tail {
        Ok(lines) => append_log_lines(app, "stderr", lines),
        Err(error) => app.status_line = error.to_string(),
    }
    app.stdout_follower = stdout_follower;
    app.stderr_follower = stderr_follower;
    app.log_service = Some(name);
    app.mode = Mode::Logs;
}

fn refresh_log_followers(app: &mut App) {
    if app.mode != Mode::Logs {
        return;
    }
    let Some(service) = app.log_service.clone() else {
        return;
    };
    let Ok((stdout_path, stderr_path)) = app.runtime.log_paths(&service) else {
        return;
    };
    let max_line_bytes = app.runtime.config().logs.max_line_bytes;
    if app.stdout_follower.is_none() {
        app.stdout_follower = FileFollower::from_start_with_limit(&stdout_path, max_line_bytes)
            .ok()
            .flatten();
    }
    if app.stderr_follower.is_none() {
        app.stderr_follower = FileFollower::from_start_with_limit(&stderr_path, max_line_bytes)
            .ok()
            .flatten();
    }
    if let Some(follower) = &mut app.stdout_follower {
        match follower.read_new_lines() {
            Ok(lines) => append_log_lines(app, "stdout", lines),
            Err(error) => app.status_line = format!("log follow error: {error}"),
        }
    }
    if let Some(follower) = &mut app.stderr_follower {
        match follower.read_new_lines() {
            Ok(lines) => append_log_lines(app, "stderr", lines),
            Err(error) => app.status_line = format!("log follow error: {error}"),
        }
    }
}

fn append_log_lines(app: &mut App, stream: &str, lines: Vec<String>) {
    for line in lines {
        let mut rendered = format!(
            "[{stream}] {}",
            app.redactor
                .redact_bounded(&line, app.runtime.config().logs.max_line_bytes)
        );
        if rendered.len() > MAX_LOG_VIEW_BYTES {
            let mut boundary = MAX_LOG_VIEW_BYTES.saturating_sub(16);
            while !rendered.is_char_boundary(boundary) {
                boundary -= 1;
            }
            rendered.truncate(boundary);
            rendered.push_str("… [truncated]");
        }
        while app.log_lines.len() >= MAX_LOG_LINES
            || app.log_bytes + rendered.len() > MAX_LOG_VIEW_BYTES
        {
            let Some(removed) = app.log_lines.pop_front() else {
                break;
            };
            app.log_bytes = app.log_bytes.saturating_sub(removed.len());
        }
        app.log_bytes += rendered.len();
        app.log_lines.push_back(rendered);
    }
}

fn clear_log_view(app: &mut App) {
    app.log_lines.clear();
    app.log_bytes = 0;
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::config::{Config, Loaded, TemplateConfig};
    use crate::core::state::{PortState, ProcessState};

    use super::*;

    fn empty_app() -> (App, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("hum-tui-test-{}-{unique}", std::process::id()));
        let loaded = Loaded {
            config: Config {
                version: 2,
                project: Some("demo".to_string()),
                templates: HashMap::from([(
                    "empty".to_string(),
                    TemplateConfig { services: vec![] },
                )]),
                ..Config::default()
            },
            base_path: root.join("hum.yaml"),
            local_path: None,
            root_dir: root.clone(),
        };
        let runtime = DetachedRuntime::with_state_root(
            "demo".to_string(),
            loaded,
            HashMap::new(),
            PathBuf::from(&root).join("state"),
        )
        .unwrap();
        (App::new(Arc::new(runtime), Some("empty".to_string())), root)
    }

    fn cleanup(app: App, root: PathBuf) {
        drop(app);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quit_requires_an_explicit_leave_choice() {
        let (mut app, root) = empty_app();
        let (action_tx, _action_rx) = mpsc::unbounded_channel();
        let (doctor_tx, _doctor_rx) = mpsc::unbounded_channel();

        handle_key(&mut app, KeyCode::Char('q'), &action_tx, &doctor_tx);
        assert_eq!(app.mode, Mode::QuitConfirm);
        assert!(!app.should_quit);

        handle_key(&mut app, KeyCode::Esc, &action_tx, &doctor_tx);
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.should_quit);

        handle_key(&mut app, KeyCode::Char('q'), &action_tx, &doctor_tx);
        handle_key(&mut app, KeyCode::Char('l'), &action_tx, &doctor_tx);
        assert!(app.should_quit);
        cleanup(app, root);
    }

    #[tokio::test]
    async fn stop_and_quit_waits_for_a_successful_stop_action() {
        let (mut app, root) = empty_app();
        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let (doctor_tx, _doctor_rx) = mpsc::unbounded_channel();
        let (monitor_tx, _monitor_rx) = mpsc::unbounded_channel();

        handle_key(&mut app, KeyCode::Char('q'), &action_tx, &doctor_tx);
        handle_key(&mut app, KeyCode::Char('s'), &action_tx, &doctor_tx);
        assert!(!app.should_quit);
        assert!(app.action_in_flight);
        handle_key(&mut app, KeyCode::Char('l'), &action_tx, &doctor_tx);
        assert!(!app.should_quit);

        let message = action_rx.recv().await.unwrap();
        apply_action(&mut app, message, &monitor_tx);

        assert!(app.should_quit);
        assert!(!app.action_in_flight);
        cleanup(app, root);
    }

    #[test]
    fn empty_templates_report_actions_instead_of_opening_blank_views() {
        let (mut app, root) = empty_app();
        let (action_tx, _action_rx) = mpsc::unbounded_channel();
        let (doctor_tx, _doctor_rx) = mpsc::unbounded_channel();

        handle_key(&mut app, KeyCode::Enter, &action_tx, &doctor_tx);

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.status_line.contains("no services"));
        cleanup(app, root);
    }

    #[test]
    fn stale_health_results_cannot_overwrite_stop_or_restart_state() {
        let (mut app, root) = empty_app();
        let mut current = crate::runtime::detached::DetachedServiceStatus {
            name: "worker".to_string(),
            process: ProcessState::Running,
            port: PortState::Closed,
            health: HealthState::Checking,
            configured_port: None,
            pid: Some(22),
            pgid: Some(22),
            exit_code: None,
            cwd: None,
            command: None,
            stdout_log: None,
            stderr_log: None,
            detail: None,
            health_detail: None,
            health_duration_ms: None,
            started_at: None,
        };
        app.statuses.insert("worker".to_string(), current.clone());
        app.health_in_flight.insert("worker".to_string());

        apply_health(
            &mut app,
            HealthUpdate {
                name: "worker".to_string(),
                pid: Some(11),
                result: Ok((HealthState::Healthy, "old process".to_string(), 1)),
            },
        );
        assert_eq!(app.statuses["worker"].health, HealthState::Checking);

        current.process = ProcessState::Stopping;
        current.pid = Some(22);
        current.health = HealthState::Unchecked;
        app.statuses.insert("worker".to_string(), current);
        app.health_in_flight.insert("worker".to_string());
        apply_health(
            &mut app,
            HealthUpdate {
                name: "worker".to_string(),
                pid: Some(22),
                result: Ok((HealthState::Healthy, "late stop result".to_string(), 1)),
            },
        );
        assert_eq!(app.statuses["worker"].health, HealthState::Unchecked);
        cleanup(app, root);
    }
}
