use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
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
    TemplateSelect,
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
    pub template: Option<String>,
    mode: Mode,
    template_cursor: usize,
    doctor_results: Vec<doctor::DoctorCheck>,
    status_line: String,
    should_quit: bool,
}

impl App {
    fn new(manager: Arc<Manager>, template: Option<String>) -> Self {
        let services = manager.service_names();
        App {
            manager,
            services,
            selected: 0,
            template,
            mode: Mode::Normal,
            template_cursor: 0,
            doctor_results: Vec::new(),
            status_line: String::new(),
            should_quit: false,
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
}

/// Launch the interactive TUI (section 8, RF-19 for refresh).
pub async fn run(manager: Arc<Manager>, template: Option<String>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, manager.clone(), template).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    manager: Arc<Manager>,
    template: Option<String>,
) -> Result<()> {
    let mut app = App::new(manager.clone(), template);
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
        Mode::TemplateSelect => {
            let mut templates: Vec<String> = app.manager.config.templates.keys().cloned().collect();
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
                    if let Some(p) = templates.get(app.template_cursor) {
                        app.template = Some(p.clone());
                        let manager = app.manager.clone();
                        let p = p.clone();
                        app.status_line = format!("starting template '{p}'...");
                        let _ = manager.start_template(&p).await;
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
                        .map(|v| v.process.is_running())
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
                app.mode = Mode::TemplateSelect;
                app.template_cursor = 0;
            }
            KeyCode::Char('d') => {
                app.doctor_results = doctor::run_with_env(
                    &app.manager.config,
                    &app.manager.root_dir,
                    app.manager.env_overrides(),
                );
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
