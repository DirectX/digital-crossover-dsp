use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::Frame;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RuntimeConfig {
    #[serde(default = "one")]
    volume: f32,
    #[serde(default = "one")]
    low_gain: f32,
    #[serde(default = "one")]
    mid_gain: f32,
    #[serde(default = "one")]
    high_gain: f32,
    #[serde(default = "default_low_cut")]
    low_cut_hz: f32,
    #[serde(default = "default_mid_cut")]
    mid_cut_hz: f32,
}

fn one() -> f32 { 1.0 }
fn default_low_cut() -> f32 { 1000.0 }
fn default_mid_cut() -> f32 { 10000.0 }

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            volume: 1.0,
            low_gain: 1.0,
            mid_gain: 1.0,
            high_gain: 1.0,
            low_cut_hz: 1000.0,
            mid_cut_hz: 10000.0,
        }
    }
}

const GAIN_MAX: f32 = 2.0;
const GAIN_MIN: f32 = 0.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Selected {
    Master,
    Low,
    Mid,
    High,
}

impl Selected {
    fn next(self) -> Self {
        match self {
            Selected::Master => Selected::Low,
            Selected::Low => Selected::Mid,
            Selected::Mid => Selected::High,
            Selected::High => Selected::Master,
        }
    }
    fn prev(self) -> Self {
        match self {
            Selected::Master => Selected::High,
            Selected::Low => Selected::Master,
            Selected::Mid => Selected::Low,
            Selected::High => Selected::Mid,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Selected::Master => "Master",
            Selected::Low => "Low",
            Selected::Mid => "Mid",
            Selected::High => "High",
        }
    }
}

fn gain_ref<'a>(cfg: &'a mut RuntimeConfig, sel: Selected) -> &'a mut f32 {
    match sel {
        Selected::Master => &mut cfg.volume,
        Selected::Low => &mut cfg.low_gain,
        Selected::Mid => &mut cfg.mid_gain,
        Selected::High => &mut cfg.high_gain,
    }
}

pub async fn run(base_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let client = reqwest::Client::new();
    let status_url = format!("{}/status", base_url);
    let config_url = format!("{}/config", base_url);
    let update_url = format!("{}/update_config", base_url);

    // Fetch initial config from server
    let mut cfg: RuntimeConfig = match client.get(&config_url).send().await {
        Ok(resp) => resp.json().await.unwrap_or_default(),
        Err(_) => RuntimeConfig::default(),
    };

    let mut selected = Selected::Master;
    let mut last_error: Option<String> = None;

    let result = loop {
        let status: Status = match client.get(&status_url).send().await {
            Ok(resp) => resp.json().await.unwrap_or_default(),
            Err(_) => Status::default(),
        };

        terminal.draw(|f| draw_ui(f, &status, &cfg, selected, last_error.as_deref()))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let fine = key.modifiers.contains(KeyModifiers::SHIFT);
                let step: f32 = if fine { 0.01 } else { 0.05 };
                let mut changed = false;

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok::<(), Box<dyn std::error::Error>>(()),
                    KeyCode::Tab | KeyCode::Down => selected = selected.next(),
                    KeyCode::BackTab | KeyCode::Up => selected = selected.prev(),
                    KeyCode::Char('m') => selected = Selected::Master,
                    KeyCode::Char('1') => selected = Selected::Master,
                    KeyCode::Char('2') => selected = Selected::Low,
                    KeyCode::Char('3') => selected = Selected::Mid,
                    KeyCode::Char('4') => selected = Selected::High,
                    KeyCode::Left | KeyCode::Char('-') => {
                        let v = gain_ref(&mut cfg, selected);
                        *v = (*v - step).clamp(GAIN_MIN, GAIN_MAX);
                        changed = true;
                    }
                    KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => {
                        let v = gain_ref(&mut cfg, selected);
                        *v = (*v + step).clamp(GAIN_MIN, GAIN_MAX);
                        changed = true;
                    }
                    KeyCode::Char('0') => {
                        *gain_ref(&mut cfg, selected) = 1.0;
                        changed = true;
                    }
                    KeyCode::Char('r') => {
                        cfg = RuntimeConfig::default();
                        changed = true;
                    }
                    _ => {}
                }

                if changed {
                    match client.post(&update_url).json(&cfg).send().await {
                        Ok(_) => last_error = None,
                        Err(e) => last_error = Some(format!("update failed: {}", e)),
                    }
                }
            }
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn draw_ui(
    f: &mut Frame,
    status: &Status,
    cfg: &RuntimeConfig,
    selected: Selected,
    last_error: Option<&str>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),  // title
            Constraint::Length(6),  // now playing
            Constraint::Length(8),  // dsp stats
            Constraint::Length(8),  // gain controls
            Constraint::Min(1),     // help/errors
        ])
        .split(f.area());

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " Digital Crossover DSP ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("Tab/↑↓: select  ←→: adjust  Shift: fine  0: reset  r: reset all  q: quit",
            Style::default().fg(Color::DarkGray)),
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

    draw_gains(f, chunks[3], cfg, selected);

    let help_text = match last_error {
        Some(err) => Line::from(Span::styled(
            format!(" {} ", err),
            Style::default().fg(Color::Red),
        )),
        None => Line::from(Span::styled(
            format!(
                " Crossover: low<{:.0}Hz  mid<{:.0}Hz  (cutoffs editable via API)",
                cfg.low_cut_hz, cfg.mid_cut_hz
            ),
            Style::default().fg(Color::DarkGray),
        )),
    };
    let footer = Paragraph::new(help_text).block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[4]);
}

fn draw_gains(f: &mut Frame, area: Rect, cfg: &RuntimeConfig, selected: Selected) {
    let outer = Block::default().title(" Gains ").borders(Borders::ALL);
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let entries = [
        (Selected::Master, cfg.volume, Color::Cyan),
        (Selected::Low, cfg.low_gain, Color::Green),
        (Selected::Mid, cfg.mid_gain, Color::Yellow),
        (Selected::High, cfg.high_gain, Color::Magenta),
    ];

    for (i, (sel, value, color)) in entries.iter().enumerate() {
        let is_sel = *sel == selected;
        let marker = if is_sel { ">" } else { " " };
        let ratio = (*value / GAIN_MAX).clamp(0.0, 1.0) as f64;
        let style = if is_sel {
            Style::default().fg(*color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(*color)
        };
        let gauge = Gauge::default()
            .gauge_style(style)
            .ratio(ratio)
            .label(format!(
                "{} {:<6} {:.2}x ({:+.1} dB)",
                marker,
                sel.label(),
                value,
                20.0 * value.max(1e-6).log10()
            ));
        f.render_widget(gauge, rows[i]);
    }
}
