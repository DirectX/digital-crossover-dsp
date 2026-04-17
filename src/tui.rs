use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::Terminal;
use serde::Deserialize;

#[derive(Default, Deserialize)]
struct Status {
    track: String,
    artist: String,
    album: String,
    playback: String,
    buffer_fill: f64,
    buffer_fill_avg: f64,
    buffer_fill_min: f64,
    buffer_fill_max: f64,
    resample_ratio: f64,
    chunks_processed: u64,
}

pub async fn run(base_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let client = reqwest::Client::new();
    let status_url = format!("{}/status", base_url);

    loop {
        let status: Status = match client.get(&status_url).send().await {
            Ok(resp) => resp.json().await.unwrap_or_default(),
            Err(_) => Status::default(),
        };

        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(7),
                    Constraint::Length(8),
                    Constraint::Min(1),
                ])
                .split(f.area());

            let title = Paragraph::new(Line::from(vec![
                Span::styled(" Digital Crossover DSP ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled("q: quit", Style::default().fg(Color::DarkGray)),
            ]))
            .block(Block::default().borders(Borders::ALL));
            f.render_widget(title, chunks[0]);

            let playback_color = match status.playback.as_str() {
                "Playing" => Color::Green,
                "Stopped" => Color::Red,
                _ => Color::Yellow,
            };
            let meta = Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("  Track: ", Style::default().fg(Color::Yellow)),
                    Span::raw(&status.track),
                ]),
                Line::from(vec![
                    Span::styled(" Artist: ", Style::default().fg(Color::Yellow)),
                    Span::raw(&status.artist),
                ]),
                Line::from(vec![
                    Span::styled("  Album: ", Style::default().fg(Color::Yellow)),
                    Span::raw(&status.album),
                ]),
                Line::from(vec![
                    Span::styled(" Status: ", Style::default().fg(Color::Yellow)),
                    Span::styled(&status.playback, Style::default().fg(playback_color)),
                ]),
            ])
            .block(Block::default().title(" Now Playing ").borders(Borders::ALL));
            f.render_widget(meta, chunks[1]);

            let fill_ratio = (status.buffer_fill / 100.0).clamp(0.0, 1.0);
            let fill_color = if status.buffer_fill < 30.0 {
                Color::Red
            } else if status.buffer_fill < 70.0 {
                Color::Yellow
            } else {
                Color::Green
            };

            let gauge = Gauge::default()
                .block(Block::default().title(" Buffer Fill ").borders(Borders::ALL))
                .gauge_style(Style::default().fg(fill_color))
                .ratio(fill_ratio)
                .label(format!("{:.1}%", status.buffer_fill));

            let stats = Paragraph::new(vec![
                Line::from(format!(
                    "  Avg: {:.1}%  Min: {:.1}%  Max: {:.1}%",
                    status.buffer_fill_avg, status.buffer_fill_min, status.buffer_fill_max
                )),
                Line::from(format!(
                    "  Resample ratio: {:.6}  Chunks: {}",
                    status.resample_ratio, status.chunks_processed
                )),
            ])
            .block(Block::default().title(" DSP Stats ").borders(Borders::ALL));

            let dsp_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Length(5)])
                .split(chunks[2]);

            f.render_widget(gauge, dsp_chunks[0]);
            f.render_widget(stats, dsp_chunks[1]);
        })?;

        if event::poll(Duration::from_millis(500))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && (key.code == KeyCode::Char('q') || key.code == KeyCode::Esc) {
                    break;
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}