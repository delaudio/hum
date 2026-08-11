use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table};
use ratatui::Frame;

use super::{App, Mode};

pub fn draw(f: &mut Frame, app: &mut App) {
    let size = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(size);

    draw_header(f, app, chunks[0]);
    draw_table(f, app, chunks[1]);
    draw_status_bar(f, app, chunks[2]);

    match app.mode {
        Mode::TemplateSelect => draw_template_select(f, app, size),
        Mode::Logs => draw_logs(f, app, size),
        Mode::Details => draw_details(f, app, size),
        Mode::Doctor => draw_doctor(f, app, size),
        Mode::QuitConfirm => draw_quit_confirm(f, app, size),
        Mode::Help => draw_help(f, size),
        Mode::Normal => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let template = app.template.clone().unwrap_or_else(|| "(none)".to_string());
    let text = format!(
        " hum — Project: {}    Template: {template}    Environment: local ",
        app.runtime.project()
    );
    let block = Block::default().borders(Borders::ALL).title(" hum ");
    f.render_widget(Paragraph::new(text).block(block), area);
}

fn process_color(status: crate::core::ProcessState) -> Color {
    use crate::core::ProcessState::*;
    match status {
        Running => Color::Green,
        Starting | Stopping => Color::Yellow,
        Exited => Color::Red,
        Missing => Color::DarkGray,
    }
}

fn health_color(status: crate::core::HealthState) -> Color {
    use crate::core::HealthState::*;
    match status {
        Healthy => Color::Green,
        Checking => Color::Yellow,
        Unhealthy => Color::Red,
        Unchecked => Color::DarkGray,
    }
}

fn draw_table(f: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(vec!["Service", "Process", "Port", "Health", "Detail"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let mut rows: Vec<Row> = app
        .services
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let style = if i == app.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let Some(status) = app.statuses.get(name) else {
                return Row::new(vec![
                    Cell::from(name.clone()),
                    Cell::from("… loading"),
                    Cell::from("unknown"),
                    Cell::from("unchecked"),
                    Cell::from("waiting for first monitor poll"),
                ])
                .style(style);
            };
            let detail = status
                .detail
                .clone()
                .or(status.health_detail.clone())
                .unwrap_or_default();
            let process_cell = Cell::from(format!(
                "{} {}",
                status.process.symbol(),
                status.process.label()
            ))
            .style(Style::default().fg(process_color(status.process)));
            let health_cell = Cell::from(status.health.label())
                .style(Style::default().fg(health_color(status.health)));
            Row::new(vec![
                Cell::from(name.clone()),
                process_cell,
                Cell::from(match status.configured_port {
                    Some(port) => format!("{port}/{}", status.port.label()),
                    None => status.port.label().to_string(),
                }),
                health_cell,
                Cell::from(detail),
            ])
            .style(style)
        })
        .collect();
    if rows.is_empty() {
        rows.push(Row::new(vec![
            Cell::from("(no services in template)"),
            Cell::from("—"),
            Cell::from("—"),
            Cell::from("—"),
            Cell::from("Select another template with p"),
        ]));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(22),
            Constraint::Percentage(20),
            Constraint::Percentage(22),
            Constraint::Percentage(14),
            Constraint::Percentage(22),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" services "));

    f.render_widget(table, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let line1 = "[space] start/stop  [r] restart  [enter] details  [l] logs";
    let line2 = if app.status_line.is_empty() {
        "[p] templates  [d] doctor  [o] open URL  [?] help  [q] quit".to_string()
    } else {
        app.status_line.clone()
    };
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

fn draw_template_select(f: &mut Frame, app: &App, area: Rect) {
    let mut templates: Vec<String> = app.runtime.config().templates.keys().cloned().collect();
    templates.sort();
    let popup = centered_rect(40, 50, area);
    f.render_widget(Clear, popup);
    let items: Vec<ListItem> = if templates.is_empty() {
        vec![ListItem::new("(no templates configured)")]
    } else {
        templates.iter().map(|p| ListItem::new(p.clone())).collect()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Select template "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ratatui::widgets::ListState::default();
    state.select((!templates.is_empty()).then_some(app.template_cursor()));
    f.render_stateful_widget(list, popup, &mut state);
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(90, 70, area);
    f.render_widget(Clear, popup);
    let Some(name) = app.selected_name() else {
        return;
    };
    let Some(status) = app.statuses.get(&name) else {
        return;
    };
    let uptime = status
        .started_at
        .and_then(|started_at| {
            chrono::Utc::now()
                .signed_duration_since(started_at)
                .to_std()
                .ok()
        })
        .map(|duration| {
            format!(
                "{:02}:{:02}:{:02}",
                duration.as_secs() / 3600,
                (duration.as_secs() / 60) % 60,
                duration.as_secs() % 60
            )
        })
        .unwrap_or_else(|| "—".to_string());
    let service = app.runtime.config().services.get(&name);
    let text = vec![
        Line::from(format!("State         {}", status.presentation().label())),
        Line::from(format!("Process       {}", status.process.label())),
        Line::from(format!(
            "PID           {}",
            status
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "—".into())
        )),
        Line::from(format!(
            "PGID          {}",
            status
                .pgid
                .map(|pgid| pgid.to_string())
                .unwrap_or_else(|| "—".into())
        )),
        Line::from(format!(
            "Exit code     {}",
            status
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "— (unavailable for detached process)".into())
        )),
        Line::from(format!("Uptime        {uptime}")),
        Line::from(format!(
            "Port          {} ({})",
            status
                .configured_port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "—".into()),
            status.port.label()
        )),
        Line::from(format!(
            "URL           {}",
            service
                .and_then(|service| service.url.clone())
                .unwrap_or_else(|| "—".into())
        )),
        Line::from(format!(
            "Health        {} ({}, {})",
            status.health.label(),
            status.health_detail.clone().unwrap_or_else(|| "—".into()),
            status
                .health_duration_ms
                .map(|duration| format!("{duration} ms"))
                .unwrap_or_else(|| "—".into())
        )),
        Line::from(format!(
            "Detail        {}",
            status.detail.clone().unwrap_or_else(|| "—".into())
        )),
        Line::from(format!(
            "Cwd           {}",
            status
                .cwd
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "—".into())
        )),
        Line::from(format!(
            "Command       {}",
            status.command.clone().unwrap_or_else(|| "—".into())
        )),
        Line::from(format!(
            "Stdout log    {}",
            status
                .stdout_log
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "—".into())
        )),
        Line::from(format!(
            "Stderr log    {}",
            status
                .stderr_log
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "—".into())
        )),
        Line::from(""),
        Line::from("[esc] back"),
    ];
    let visible_height = usize::from(popup.height.saturating_sub(2));
    let max_scroll = text
        .len()
        .saturating_sub(visible_height)
        .min(usize::from(u16::MAX)) as u16;
    f.render_widget(
        Paragraph::new(text)
            .scroll((app.details_scroll.min(max_scroll), app.details_horizontal))
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {name} (j/k vertical, h/l horizontal, esc to close) "
            ))),
        popup,
    );
}

fn draw_logs(f: &mut Frame, app: &mut App, area: Rect) {
    let popup = centered_rect(90, 80, area);
    f.render_widget(Clear, popup);
    let Some(name) = app.log_service.as_deref() else {
        return;
    };
    let lines: Vec<Line> = app
        .log_lines
        .iter()
        .filter(|line| app.log_search.is_empty() || line.contains(&app.log_search))
        .map(|line| Line::from(line.as_str()))
        .collect();
    let search = if app.log_searching {
        format!(" search: /{}_ ", app.log_search)
    } else if app.log_search.is_empty() {
        String::new()
    } else {
        format!(" filter: /{} ", app.log_search)
    };
    let visible_height = usize::from(popup.height.saturating_sub(2));
    let visible_width = usize::from(popup.width.saturating_sub(2));
    app.log_visible_height = visible_height;
    let max_scroll = lines.len().saturating_sub(visible_height);
    let max_horizontal = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or_default()
        .saturating_sub(visible_width)
        .min(usize::from(u16::MAX)) as u16;
    app.log_horizontal = app.log_horizontal.min(max_horizontal);
    let from_bottom = app.log_scroll_from_bottom.min(max_scroll);
    let scroll = max_scroll
        .saturating_sub(from_bottom)
        .min(usize::from(u16::MAX)) as u16;
    let state = if app.log_follow { "LIVE" } else { "PAUSED" };
    let history = if app.log_history_exhausted {
        " oldest loaded"
    } else {
        " Home: older"
    };
    f.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, app.log_horizontal))
            .block(
            Block::default().borders(Borders::ALL).title(format!(
                " {name} — logs [{state}] (j/k: vertical, h/l: horizontal, 0: left, PgUp/PgDn, End: live,{history}, /: search, c: clear, esc: close){search}"
            )),
        ),
        popup,
    );
}

fn draw_doctor(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(80, 80, area);
    f.render_widget(Clear, popup);
    let mut lines: Vec<Line> = app
        .doctor_results
        .iter()
        .map(|r| {
            let prefix = if r.ok { "✓" } else { "✗" };
            let color = if r.ok { Color::Green } else { Color::Red };
            let scope = r
                .scope
                .clone()
                .map(|s| format!("[{s}] "))
                .unwrap_or_default();
            let detail = r
                .detail
                .clone()
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            Line::from(Span::styled(
                format!("{prefix} {scope}{}{detail}", r.label),
                Style::default().fg(color),
            ))
        })
        .collect();
    if app.doctor_in_flight {
        lines.insert(0, Line::from("… diagnostics running in background"));
    }
    let visible_height = usize::from(popup.height.saturating_sub(2));
    let max_scroll = lines
        .len()
        .saturating_sub(visible_height)
        .min(usize::from(u16::MAX)) as u16;
    f.render_widget(
        Paragraph::new(lines)
            .scroll((app.doctor_scroll.min(max_scroll), 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" doctor (j/k or PgUp/PgDn, esc to close) "),
            ),
        popup,
    );
}

fn draw_quit_confirm(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(62, 36, area);
    f.render_widget(Clear, popup);
    let template = app.template.as_deref().unwrap_or("(none)");
    let text = vec![
        Line::from("Choose explicitly what should happen to detached services:"),
        Line::from(""),
        Line::from("[l] leave services running and quit"),
        Line::from(format!("[s] stop template '{template}' and quit")),
        Line::from("[esc] cancel"),
    ];
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" quit hum ")),
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
        Line::from("p           select template"),
        Line::from("d           doctor"),
        Line::from("o           open URL"),
        Line::from("?           help"),
        Line::from("q           quit"),
    ];
    f.render_widget(
        Paragraph::new(text).alignment(Alignment::Left).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" help (esc to close) "),
        ),
        popup,
    );
}
