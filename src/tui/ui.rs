use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table};
use ratatui::Frame;

use super::{App, Mode};

pub fn draw(f: &mut Frame, app: &App) {
    let size = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(2)])
        .split(size);

    draw_header(f, app, chunks[0]);
    draw_table(f, app, chunks[1]);
    draw_status_bar(f, chunks[2]);

    match app.mode {
        Mode::ProfileSelect => draw_profile_select(f, app, size),
        Mode::Logs => draw_logs(f, app, size),
        Mode::Details => draw_details(f, app, size),
        Mode::Doctor => draw_doctor(f, app, size),
        Mode::Help => draw_help(f, size),
        Mode::ConfirmQuit => draw_confirm_quit(f, app, size),
        Mode::Normal => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let profile = app.profile.clone().unwrap_or_else(|| "(none)".to_string());
    let text = format!(" hum — Profile: {profile}    Environment: local ");
    let block = Block::default().borders(Borders::ALL).title(" hum ");
    f.render_widget(Paragraph::new(text).block(block), area);
}

fn status_color(status: crate::core::ServiceStatus) -> Color {
    use crate::core::ServiceStatus::*;
    match status {
        Healthy | Running => Color::Green,
        Starting | Stopping => Color::Yellow,
        Unhealthy | Blocked => Color::Rgb(255, 165, 0),
        Failed => Color::Red,
        Stopped => Color::DarkGray,
    }
}

fn draw_table(f: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(vec!["Service", "Status", "Port", "Health / Detail"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .services
        .iter()
        .enumerate()
        .filter_map(|(i, name)| {
            let view = app.manager.view(name)?;
            let detail = view
                .blocked_reason
                .clone()
                .or(view.health_detail.clone())
                .unwrap_or_default();
            let style = if i == app.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let status_cell = Cell::from(format!(
                "{} {}",
                view.status.symbol(),
                view.status.label()
            ))
            .style(Style::default().fg(status_color(view.status)));
            Some(
                Row::new(vec![
                    Cell::from(name.clone()),
                    status_cell,
                    Cell::from(view.port.map(|p| p.to_string()).unwrap_or_else(|| "—".into())),
                    Cell::from(detail),
                ])
                .style(style),
            )
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(30),
            Constraint::Percentage(20),
            Constraint::Percentage(10),
            Constraint::Percentage(40),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" services "));

    f.render_widget(table, area);
}

fn draw_status_bar(f: &mut Frame, area: Rect) {
    let line1 = "[space] start/stop  [r] restart  [enter] details  [l] logs";
    let line2 = "[p] profiles  [d] doctor  [o] open URL  [?] help  [q] quit";
    let text = vec![Line::from(line1), Line::from(line2)];
    f.render_widget(Paragraph::new(text), area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn draw_profile_select(f: &mut Frame, app: &App, area: Rect) {
    let mut profiles: Vec<String> = app.manager.config.profiles.keys().cloned().collect();
    profiles.sort();
    let popup = centered_rect(40, 50, area);
    f.render_widget(Clear, popup);
    let items: Vec<ListItem> = profiles
        .iter()
        .map(|p| ListItem::new(p.clone()))
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Select profile "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(app.profile_cursor()));
    f.render_stateful_widget(list, popup, &mut state);
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(60, 60, area);
    f.render_widget(Clear, popup);
    let Some(name) = app.selected_name() else { return };
    let Some(view) = app.manager.view(&name) else { return };
    let uptime = view
        .uptime
        .map(|d| format!("{:02}:{:02}:{:02}", d.as_secs() / 3600, (d.as_secs() / 60) % 60, d.as_secs() % 60))
        .unwrap_or_else(|| "—".to_string());
    let text = vec![
        Line::from(format!("Status        {}", view.status.label())),
        Line::from(format!("PID           {}", view.pid.map(|p| p.to_string()).unwrap_or_else(|| "—".into()))),
        Line::from(format!("Uptime        {uptime}")),
        Line::from(format!("Port          {}", view.port.map(|p| p.to_string()).unwrap_or_else(|| "—".into()))),
        Line::from(format!("URL           {}", view.url.clone().unwrap_or_else(|| "—".into()))),
        Line::from(format!("Health        {}", view.health_detail.clone().unwrap_or_else(|| "—".into()))),
        Line::from(format!("Blocked       {}", view.blocked_reason.clone().unwrap_or_else(|| "—".into()))),
        Line::from(""),
        Line::from("[esc] back"),
    ];
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(format!(" {name} "))),
        popup,
    );
}

fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(90, 80, area);
    f.render_widget(Clear, popup);
    let Some(name) = app.selected_name() else { return };
    let lines: Vec<Line> = app
        .manager
        .logs(&name)
        .map(|buf| {
            buf.tail(200)
                .iter()
                .map(|l| Line::from(format!("{}  [{}] {}", l.timestamp.format("%H:%M:%S"), l.stream.label(), l.content)))
                .collect()
        })
        .unwrap_or_default();
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(format!(" {name} — logs (esc to close) "))),
        popup,
    );
}

fn draw_doctor(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(80, 80, area);
    f.render_widget(Clear, popup);
    let lines: Vec<Line> = app
        .doctor_results
        .iter()
        .map(|r| {
            let prefix = if r.ok { "✓" } else { "✗" };
            let color = if r.ok { Color::Green } else { Color::Red };
            let scope = r.scope.clone().map(|s| format!("[{s}] ")).unwrap_or_default();
            let detail = r.detail.clone().map(|d| format!(": {d}")).unwrap_or_default();
            Line::from(Span::styled(format!("{prefix} {scope}{}{detail}", r.label), Style::default().fg(color)))
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" doctor (esc to close) ")),
        popup,
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let popup = centered_rect(50, 60, area);
    f.render_widget(Clear, popup);
    let text = vec![
        Line::from("↑ / k       previous service"),
        Line::from("↓ / j       next service"),
        Line::from("space       start / stop"),
        Line::from("r           restart"),
        Line::from("enter       details"),
        Line::from("l           logs"),
        Line::from("p           select profile"),
        Line::from("d           doctor"),
        Line::from("o           open URL"),
        Line::from("?           help"),
        Line::from("q           quit"),
    ];
    f.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Left)
            .block(Block::default().borders(Borders::ALL).title(" help (esc to close) ")),
        popup,
    );
}

fn draw_confirm_quit(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(50, 20, area);
    f.render_widget(Clear, popup);
    let _ = app;
    let text = Paragraph::new("Stop all running services before quitting? [Y/n]")
        .block(Block::default().borders(Borders::ALL).title(" quit "));
    f.render_widget(text, popup);
}
