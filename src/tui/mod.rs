use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
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
use crate::runtime::logs::{tail_file, FileFollower};

mod ui;

const EVENT_TICK: Duration = Duration::from_millis(250);
const RUNTIME_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_LOG_LINES: usize = 500;

#[derive(PartialEq)]
enum Mode {
    Normal,
    TemplateSelect,
    Logs,
    Details,
    Doctor,
    Help,
}

pub struct App {
    pub runtime: Arc<DetachedRuntime>,
    pub statuses: HashMap<String, DetachedServiceStatus>,
    pub services: Vec<String>,
    pub selected: usize,
    pub template: Option<String>,
    pub log_lines: VecDeque<String>,
    mode: Mode,
    template_cursor: usize,
    doctor_results: Vec<doctor::DoctorCheck>,
    status_line: String,
    should_quit: bool,
    health_due: HashMap<String, Instant>,
    stdout_follower: Option<FileFollower>,
    stderr_follower: Option<FileFollower>,
    log_service: Option<String>,
    active_poll: Option<u64>,
    poll_task: Option<tokio::task::JoinHandle<()>>,
    poll_guard: Arc<tokio::sync::Mutex<()>>,
    next_poll_id: u64,
    runtime_due: Instant,
    health_in_flight: HashSet<String>,
    action_in_flight: bool,
}

impl App {
    fn new(runtime: Arc<DetachedRuntime>, template: Option<String>) -> Self {
        let services = template
            .as_deref()
            .and_then(|name| crate::core::graph::services_for_template(runtime.config(), name).ok())
            .unwrap_or_default();
        App {
            runtime,
            statuses: HashMap::new(),
            services,
            selected: 0,
            template,
            log_lines: VecDeque::with_capacity(MAX_LOG_LINES),
            mode: Mode::Normal,
            template_cursor: 0,
            doctor_results: Vec::new(),
            status_line: String::new(),
            should_quit: false,
            health_due: HashMap::new(),
            stdout_follower: None,
            stderr_follower: None,
            log_service: None,
            active_poll: None,
            poll_task: None,
            poll_guard: Arc::new(tokio::sync::Mutex::new(())),
            next_poll_id: 0,
            runtime_due: Instant::now(),
            health_in_flight: HashSet::new(),
            action_in_flight: false,
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
}

/// Launch the interactive monitor. Quitting the TUI never stops services.
pub async fn run(runtime: Arc<DetachedRuntime>, template: Option<String>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, runtime, template).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
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
            maybe_event = events.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_event {
                    if key.kind == KeyEventKind::Press {
                        handle_key(&mut app, key.code, &action_tx);
                    }
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
    app.status_line = match message.result {
        Ok(detail) => detail,
        Err(error) => format!("action failed: {error}"),
    };
    app.invalidate_poll();
    schedule_poll(app, monitor_tx);
}

fn handle_key(app: &mut App, code: KeyCode, action_tx: &mpsc::UnboundedSender<ActionMessage>) {
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
                    }
                    app.mode = Mode::Normal;
                }
                KeyCode::Esc => app.mode = Mode::Normal,
                _ => {}
            }
        }
        Mode::Logs => match code {
            KeyCode::Esc | KeyCode::Char('q') => app.mode = Mode::Normal,
            KeyCode::Char('c') => app.log_lines.clear(),
            _ => {}
        },
        Mode::Details | Mode::Doctor | Mode::Help => {
            if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                app.mode = Mode::Normal;
            }
        }
        Mode::Normal => match code {
            KeyCode::Char('q') => {
                if app.action_in_flight {
                    app.status_line =
                        "wait for the explicit start/stop/restart action to finish".to_string();
                } else {
                    app.should_quit = true;
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
                                if report.succeeded() {
                                    Ok(format!("service '{action_name}' stopped"))
                                } else {
                                    Err(report.failures[0].detail.clone())
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
                                runtime
                                    .start_services(std::slice::from_ref(&action_name))
                                    .await
                                    .map(|_| format!("service '{action_name}' started"))
                                    .map_err(|error| error.to_string())
                            },
                        );
                    }
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
                }
            }
            KeyCode::Enter => app.mode = Mode::Details,
            KeyCode::Char('l') => open_logs(app),
            KeyCode::Char('p') => {
                app.mode = Mode::TemplateSelect;
                app.template_cursor = 0;
            }
            KeyCode::Char('d') => {
                app.doctor_results = doctor::run_with_env(
                    app.runtime.config(),
                    app.runtime.root_dir(),
                    app.runtime.env_overrides(),
                );
                app.mode = Mode::Doctor;
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
                        let _ = open::that(url);
                    }
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
    if app.action_in_flight {
        app.status_line = "another action is still running".to_string();
        return;
    }
    app.action_in_flight = true;
    app.status_line = pending;
    app.invalidate_poll();
    let tx = tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(ActionMessage {
            result: action.await,
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
    app.log_lines.clear();
    match tail_file(&stdout_path, 200) {
        Ok(lines) => append_log_lines(app, "stdout", lines),
        Err(error) => app.status_line = error.to_string(),
    }
    match tail_file(&stderr_path, 200) {
        Ok(lines) => append_log_lines(app, "stderr", lines),
        Err(error) => app.status_line = error.to_string(),
    }
    app.stdout_follower = FileFollower::from_end(&stdout_path).ok().flatten();
    app.stderr_follower = FileFollower::from_end(&stderr_path).ok().flatten();
    app.log_service = Some(name);
    app.mode = Mode::Logs;
}

fn refresh_log_followers(app: &mut App) {
    if app.mode != Mode::Logs {
        return;
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
        if app.log_lines.len() == MAX_LOG_LINES {
            app.log_lines.pop_front();
        }
        app.log_lines.push_back(format!("[{stream}] {line}"));
    }
}
