use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::core::Manager;
use crate::doctor;

mod ui;

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

#[derive(PartialEq)]
enum Mode {
    Normal,
    ProfileSelect,
    Logs,
    Details,
    Doctor,
    Help,
    ConfirmQuit,
}

pub struct App {
    pub manager: Arc<Manager>,
    pub services: Vec<String>,
    pub selected: usize,
    pub profile: Option<String>,
    mode: Mode,
    profile_cursor: usize,
    doctor_results: Vec<doctor::DoctorCheck>,
    status_line: String,
    should_quit: bool,
}

impl App {
    fn new(manager: Arc<Manager>) -> Self {
        let services = manager.service_names();
        App {
            manager,
            services,
            selected: 0,
            profile: None,
            mode: Mode::Normal,
            profile_cursor: 0,
            doctor_results: Vec::new(),
            status_line: String::new(),
            should_quit: false,
        }
    }

    fn selected_name(&self) -> Option<String> {
        self.services.get(self.selected).cloned()
    }

    pub(crate) fn profile_cursor(&self) -> usize {
        self.profile_cursor
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
}

/// Launch the interactive TUI (section 8, RF-19 for refresh).
pub async fn run(manager: Arc<Manager>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, manager.clone()).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    manager: Arc<Manager>,
) -> Result<()> {
    let mut app = App::new(manager.clone());
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(REFRESH_INTERVAL);

    loop {
        app.manager.reap_exited();
        terminal.draw(|f| ui::draw(f, &app))?;

        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            _ = ticker.tick() => { /* just redraw with fresh state */ }
            maybe_event = events.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_event {
                    if key.kind == KeyEventKind::Press {
                        handle_key(&mut app, key.code).await;
                    }
                }
            }
        }
    }
}

async fn handle_key(app: &mut App, code: KeyCode) {
    match app.mode {
        Mode::ConfirmQuit => match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let _ = app.manager.stop_all().await;
                app.should_quit = true;
            }
            _ => app.should_quit = true, // n or anything else: quit without stopping
        },
        Mode::ProfileSelect => {
            let mut profiles: Vec<String> = app.manager.config.profiles.keys().cloned().collect();
            profiles.sort();
            match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.profile_cursor == 0 {
                        app.profile_cursor = profiles.len().saturating_sub(1);
                    } else {
                        app.profile_cursor -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !profiles.is_empty() {
                        app.profile_cursor = (app.profile_cursor + 1) % profiles.len();
                    }
                }
                KeyCode::Enter => {
                    if let Some(p) = profiles.get(app.profile_cursor) {
                        app.profile = Some(p.clone());
                        let manager = app.manager.clone();
                        let p = p.clone();
                        app.status_line = format!("starting profile '{p}'...");
                        let _ = manager.start_profile(&p).await;
                    }
                    app.mode = Mode::Normal;
                }
                KeyCode::Esc => app.mode = Mode::Normal,
                _ => {}
            }
        }
        Mode::Logs | Mode::Details | Mode::Doctor | Mode::Help => {
            if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                app.mode = Mode::Normal;
            }
        }
        Mode::Normal => match code {
            KeyCode::Char('q') => {
                if app.manager.any_running() {
                    app.mode = Mode::ConfirmQuit;
                } else {
                    app.should_quit = true;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => app.prev(),
            KeyCode::Down | KeyCode::Char('j') => app.next(),
            KeyCode::Char(' ') => {
                if let Some(name) = app.selected_name() {
                    let manager = app.manager.clone();
                    let running = manager
                        .view(&name)
                        .map(|v| v.status.is_started())
                        .unwrap_or(false);
                    if running {
                        let _ = manager.stop_service(&name).await;
                    } else {
                        let _ = manager.start_services(&[name]).await;
                    }
                }
            }
            KeyCode::Char('r') => {
                if let Some(name) = app.selected_name() {
                    let manager = app.manager.clone();
                    let _ = manager.restart_service(&name).await;
                }
            }
            KeyCode::Enter => app.mode = Mode::Details,
            KeyCode::Char('l') => app.mode = Mode::Logs,
            KeyCode::Char('p') => {
                app.mode = Mode::ProfileSelect;
                app.profile_cursor = 0;
            }
            KeyCode::Char('d') => {
                app.doctor_results = doctor::run(&app.manager.config, &app.manager.root_dir);
                app.mode = Mode::Doctor;
            }
            KeyCode::Char('o') => {
                if let Some(name) = app.selected_name() {
                    if let Some(view) = app.manager.view(&name) {
                        if let Some(url) = view.url {
                            let _ = open::that(url);
                        }
                    }
                }
            }
            KeyCode::Char('?') => app.mode = Mode::Help,
            _ => {}
        },
    }
}
