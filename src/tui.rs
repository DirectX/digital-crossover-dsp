use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::Frame;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as WsMessage};

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
    output_rate: u32,
    output_format: String,
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
    #[serde(default)]
    low_mute: bool,
    #[serde(default)]
    mid_mute: bool,
    #[serde(default)]
    high_mute: bool,
    #[serde(default)]
    low_solo: bool,
    #[serde(default)]
    mid_solo: bool,
    #[serde(default)]
    high_solo: bool,
    #[serde(default)]
    low_bypass: bool,
    #[serde(default)]
    mid_bypass: bool,
    #[serde(default)]
    high_bypass: bool,
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
            low_mute: false,
            mid_mute: false,
            high_mute: false,
            low_solo: false,
            mid_solo: false,
            high_solo: false,
            low_bypass: false,
            mid_bypass: false,
            high_bypass: false,
        }
    }
}

const GAIN_MAX: f32 = 2.0;
const GAIN_MIN: f32 = 0.0;
const FREQ_MIN: f32 = 20.0;
const FREQ_MAX: f32 = 20000.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Selected {
    Master,
    Low,
    Mid,
    High,
    LowCut,
    MidCut,
}

impl Selected {
    fn next(self) -> Self {
        match self {
            Selected::Master => Selected::Low,
            Selected::Low   => Selected::Mid,
            Selected::Mid   => Selected::High,
            Selected::High  => Selected::LowCut,
            Selected::LowCut => Selected::MidCut,
            Selected::MidCut => Selected::Master,
        }
    }
    fn prev(self) -> Self {
        match self {
            Selected::Master => Selected::MidCut,
            Selected::Low    => Selected::Master,
            Selected::Mid    => Selected::Low,
            Selected::High   => Selected::Mid,
            Selected::LowCut => Selected::High,
            Selected::MidCut => Selected::LowCut,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Selected::Master => "Master",
            Selected::Low    => "Low",
            Selected::Mid    => "Mid",
            Selected::High   => "High",
            Selected::LowCut => "Low cut",
            Selected::MidCut => "Mid cut",
        }
    }
    fn is_freq(self) -> bool {
        matches!(self, Selected::LowCut | Selected::MidCut)
    }
}

fn gain_ref<'a>(cfg: &'a mut RuntimeConfig, sel: Selected) -> &'a mut f32 {
    match sel {
        Selected::Master => &mut cfg.volume,
        Selected::Low    => &mut cfg.low_gain,
        Selected::Mid    => &mut cfg.mid_gain,
        Selected::High   => &mut cfg.high_gain,
        _ => unreachable!(),
    }
}

fn freq_ref<'a>(cfg: &'a mut RuntimeConfig, sel: Selected) -> &'a mut f32 {
    match sel {
        Selected::LowCut => &mut cfg.low_cut_hz,
        Selected::MidCut => &mut cfg.mid_cut_hz,
        _ => unreachable!(),
    }
}

fn default_freq(sel: Selected) -> f32 {
    match sel {
        Selected::LowCut => 1000.0,
        Selected::MidCut => 10000.0,
        _ => unreachable!(),
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

    // Shared FFT bins updated by background WS task
    let fft_bins: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let fft_bins_task = fft_bins.clone();
    let ws_url = base_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1)
        + "/ws/fft";
    tokio::spawn(async move {
        loop {
            match connect_async(&ws_url).await {
                Ok((ws_stream, _)) => {
                    let (_, mut read) = ws_stream.split();
                    while let Some(Ok(msg)) = read.next().await {
                        if let WsMessage::Text(text) = msg {
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                                if let Some(arr) = val["bins"].as_array() {
                                    let bins: Vec<f32> = arr
                                        .iter()
                                        .filter_map(|v| v.as_f64().map(|f| f as f32))
                                        .collect();
                                    *fft_bins_task.lock().unwrap() = bins;
                                }
                            }
                        }
                    }
                }
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    let mut selected = Selected::Master;
    let mut last_error: Option<String> = None;

    let result = loop {
        let status: Status = match client.get(&status_url).send().await {
            Ok(resp) => resp.json().await.unwrap_or_default(),
            Err(_) => Status::default(),
        };

        let bins_snapshot = fft_bins.lock().unwrap().clone();
        terminal.draw(|f| draw_ui(f, &status, &cfg, selected, last_error.as_deref(), &bins_snapshot))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let fine = key.modifiers.contains(KeyModifiers::SHIFT);
                let gain_step: f32 = if fine { 0.01 } else { 0.05 };
                let freq_step: f32 = if fine { 5.0 } else { 50.0 };
                let mut changed = false;

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok::<(), Box<dyn std::error::Error>>(()),
                    KeyCode::Tab | KeyCode::Down => selected = selected.next(),
                    KeyCode::BackTab | KeyCode::Up => selected = selected.prev(),
                    KeyCode::Char('1') => selected = Selected::Master,
                    KeyCode::Char('2') => selected = Selected::Low,
                    KeyCode::Char('3') => selected = Selected::Mid,
                    KeyCode::Char('4') => selected = Selected::High,
                    KeyCode::Char('5') => selected = Selected::LowCut,
                    KeyCode::Char('6') => selected = Selected::MidCut,
                    KeyCode::Char('m') => {
                        match selected {
                            Selected::Low  => { cfg.low_mute  = !cfg.low_mute;  changed = true; }
                            Selected::Mid  => { cfg.mid_mute  = !cfg.mid_mute;  changed = true; }
                            Selected::High => { cfg.high_mute = !cfg.high_mute; changed = true; }
                            _ => {}
                        }
                    }
                    KeyCode::Char('s') => {
                        match selected {
                            Selected::Low  => {
                                let new = !cfg.low_solo;
                                cfg.low_solo = new; cfg.mid_solo = false; cfg.high_solo = false;
                                changed = true;
                            }
                            Selected::Mid  => {
                                let new = !cfg.mid_solo;
                                cfg.low_solo = false; cfg.mid_solo = new; cfg.high_solo = false;
                                changed = true;
                            }
                            Selected::High => {
                                let new = !cfg.high_solo;
                                cfg.low_solo = false; cfg.mid_solo = false; cfg.high_solo = new;
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                    KeyCode::Char('b') => {
                        match selected {
                            Selected::Low  => { cfg.low_bypass  = !cfg.low_bypass;  changed = true; }
                            Selected::Mid  => { cfg.mid_bypass  = !cfg.mid_bypass;  changed = true; }
                            Selected::High => { cfg.high_bypass = !cfg.high_bypass; changed = true; }
                            _ => {}
                        }
                    }
                    KeyCode::Left | KeyCode::Char('-') => {
                        if selected.is_freq() {
                            let lo = if selected == Selected::LowCut {
                                FREQ_MIN
                            } else {
                                cfg.low_cut_hz + 100.0
                            };
                            let v = freq_ref(&mut cfg, selected);
                            *v = (*v - freq_step).max(lo);
                        } else {
                            let v = gain_ref(&mut cfg, selected);
                            *v = (*v - gain_step).clamp(GAIN_MIN, GAIN_MAX);
                        }
                        changed = true;
                    }
                    KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => {
                        if selected.is_freq() {
                            let hi = if selected == Selected::MidCut {
                                FREQ_MAX
                            } else {
                                cfg.mid_cut_hz - 100.0
                            };
                            let v = freq_ref(&mut cfg, selected);
                            *v = (*v + freq_step).min(hi.max(FREQ_MIN));
                        } else {
                            let v = gain_ref(&mut cfg, selected);
                            *v = (*v + gain_step).clamp(GAIN_MIN, GAIN_MAX);
                        }
                        changed = true;
                    }
                    KeyCode::Char('0') => {
                        if selected.is_freq() {
                            *freq_ref(&mut cfg, selected) = default_freq(selected);
                        } else {
                            *gain_ref(&mut cfg, selected) = 1.0;
                        }
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
    fft_bins: &[f32],
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),  // title
            Constraint::Length(6),  // now playing
            Constraint::Length(8),  // dsp stats
            Constraint::Length(6),  // gain controls
            Constraint::Length(4),  // crossover
            Constraint::Min(8),     // spectrum
            Constraint::Length(3),  // footer/errors
        ])
        .split(f.area());

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " Digital Crossover DSP ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "1-4: gains  5-6: cutoffs  Tab/↑↓: select  ←→: adjust  Shift: fine  0: reset  r: all  q: quit",
            Style::default().fg(Color::DarkGray),
        ),
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
    let fill_color = if (40.0..=60.0).contains(&status.buffer_fill) {
        Color::Green
    } else if (20.0..=80.0).contains(&status.buffer_fill) {
        Color::Yellow
    } else {
        Color::Red
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
        Line::from(vec![
            Span::styled("  Output: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if status.output_rate > 0 {
                    format!("{} Hz · {} · 6ch", status.output_rate, status.output_format)
                } else {
                    "—".to_string()
                },
                Style::default().fg(Color::Cyan),
            ),
        ]),
    ])
    .block(Block::default().title(" DSP Stats ").borders(Borders::ALL));

    let dsp_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(5)])
        .split(chunks[2]);

    f.render_widget(gauge, dsp_chunks[0]);
    f.render_widget(stats, dsp_chunks[1]);

    draw_gains(f, chunks[3], cfg, selected);
    draw_crossover(f, chunks[4], cfg, selected);
    draw_spectrum(f, chunks[5], fft_bins, status.output_rate, cfg.low_cut_hz, cfg.mid_cut_hz);

    let footer_text = match last_error {
        Some(err) => Line::from(Span::styled(
            format!(" {} ", err),
            Style::default().fg(Color::Red),
        )),
        None => Line::from(Span::styled(
            " 1-4: gains  5-6: cutoffs  m: mute  s: solo  b: bypass  Tab/↑↓: sel  ←→: adjust  Shift: fine  0: reset  r: all",
            Style::default().fg(Color::DarkGray),
        )),
    };
    let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[6]);
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
        (Selected::Master, cfg.volume,    Color::Cyan,    false, false, false),
        (Selected::Low,    cfg.low_gain,  Color::Green,   cfg.low_mute,  cfg.low_solo,  cfg.low_bypass),
        (Selected::Mid,    cfg.mid_gain,  Color::Yellow,  cfg.mid_mute,  cfg.mid_solo,  cfg.mid_bypass),
        (Selected::High,   cfg.high_gain, Color::Magenta, cfg.high_mute, cfg.high_solo, cfg.high_bypass),
    ];

    for (i, (sel, value, color, mute, solo, bypass)) in entries.iter().enumerate() {
        let is_sel = *sel == selected;
        let marker = if is_sel { ">" } else { " " };
        let ratio = (*value / GAIN_MAX).clamp(0.0, 1.0) as f64;
        let gauge_style = if is_sel {
            Style::default().fg(*color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(*color)
        };

        let has_flags = matches!(sel, Selected::Low | Selected::Mid | Selected::High);

        if has_flags {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(14)])
                .split(rows[i]);

            let gauge = Gauge::default()
                .gauge_style(gauge_style)
                .ratio(ratio)
                .label(format!(
                    "{} {:<6} {:.2}x ({:+.1} dB)",
                    marker, sel.label(), value,
                    20.0 * value.max(1e-6).log10()
                ));
            f.render_widget(gauge, cols[0]);

            let mu_style = if *mute   { Style::default().fg(Color::Red)    } else { Style::default().fg(Color::DarkGray) };
            let so_style = if *solo   { Style::default().fg(Color::Yellow) } else { Style::default().fg(Color::DarkGray) };
            let by_style = if *bypass { Style::default().fg(Color::Cyan)   } else { Style::default().fg(Color::DarkGray) };
            let badges = Paragraph::new(Line::from(vec![
                Span::styled(" [", Style::default().fg(Color::DarkGray)),
                Span::styled("M", mu_style),
                Span::styled("][", Style::default().fg(Color::DarkGray)),
                Span::styled("S", so_style),
                Span::styled("][", Style::default().fg(Color::DarkGray)),
                Span::styled("B", by_style),
                Span::styled("]", Style::default().fg(Color::DarkGray)),
            ]));
            f.render_widget(badges, cols[1]);
        } else {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(14)])
                .split(rows[i]);
            let gauge = Gauge::default()
                .gauge_style(gauge_style)
                .ratio(ratio)
                .label(format!(
                    "{} {:<6} {:.2}x ({:+.1} dB)",
                    marker, sel.label(), value,
                    20.0 * value.max(1e-6).log10()
                ));
            f.render_widget(gauge, cols[0]);
        }
    }
}

fn draw_crossover(f: &mut Frame, area: Rect, cfg: &RuntimeConfig, selected: Selected) {
    let outer = Block::default().title(" Crossover Frequencies ").borders(Borders::ALL);
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    let entries = [
        (Selected::LowCut, cfg.low_cut_hz,  Color::Green),
        (Selected::MidCut, cfg.mid_cut_hz,  Color::Yellow),
    ];

    for (i, (sel, value, color)) in entries.iter().enumerate() {
        let is_sel = *sel == selected;
        let marker = if is_sel { ">" } else { " " };
        let log_lo = FREQ_MIN.log10() as f64;
        let log_hi = FREQ_MAX.log10() as f64;
        let log_val = (*value as f64).log10().clamp(log_lo, log_hi);
        let ratio = ((log_val - log_lo) / (log_hi - log_lo)).clamp(0.0, 1.0);
        let style = if is_sel {
            Style::default().fg(*color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(*color)
        };
        let gauge = Gauge::default()
            .gauge_style(style)
            .ratio(ratio)
            .label(format!(
                "{} {:<8} {:.0} Hz",
                marker,
                sel.label(),
                value,
            ));
        f.render_widget(gauge, rows[i]);
    }
}

fn draw_spectrum(
    f: &mut Frame,
    area: Rect,
    bins: &[f32],
    sample_rate: u32,
    low_cut_hz: f32,
    mid_cut_hz: f32,
) {
    let title = if sample_rate > 0 {
        format!(" Spectrum  (20 Hz – {} kHz) ", sample_rate / 2 / 1000)
    } else {
        " Spectrum ".to_string()
    };
    let outer = Block::default().title(title).borders(Borders::ALL);
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    if bins.is_empty() || inner.width == 0 || inner.height == 0 {
        f.render_widget(
            Paragraph::new("Waiting for audio...")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let cols = inner.width as usize;
    let rows = inner.height as usize;
    let n_bins = bins.len();

    const DB_MIN: f32 = -80.0;
    const DB_MAX: f32 = 0.0;

    // Compute which display column the crossover frequencies fall on.
    // bin_for_freq = freq * n_bins * 2 / sample_rate  (linear bin index)
    // col_for_bin  = log(bin) / log(n_bins) * cols   (log-index mapping)
    let freq_to_col = |hz: f32| -> usize {
        if sample_rate == 0 || hz <= 0.0 {
            return 0;
        }
        let bin = (hz * (n_bins * 2) as f32 / sample_rate as f32).clamp(1.0, n_bins as f32);
        let t = bin.log(n_bins as f32);
        (t * cols as f32) as usize
    };

    let low_col = freq_to_col(low_cut_hz);
    let mid_col = freq_to_col(mid_cut_hz);

    // Map each display column to an FFT bin range using log-index scale
    let col_db: Vec<f32> = (0..cols)
        .map(|col| {
            let t0 = col as f64 / cols as f64;
            let t1 = (col + 1) as f64 / cols as f64;
            let b0 = (n_bins as f64).powf(t0) as usize;
            let b1 = ((n_bins as f64).powf(t1) as usize).min(n_bins);
            let b0 = b0.min(b1.saturating_sub(1));
            let b1 = b1.max(b0 + 1).min(n_bins);
            bins[b0..b1]
                .iter()
                .cloned()
                .fold(f32::NEG_INFINITY, f32::max)
        })
        .collect();

    // Precompute per-column band colour (matches gain bar colours)
    let col_color: Vec<Color> = (0..cols)
        .map(|col| {
            if col < low_col {
                Color::Green   // Low band
            } else if col < mid_col {
                Color::Yellow  // Mid band
            } else {
                Color::Magenta // High band
            }
        })
        .collect();

    // Sub-cell vertical resolution using Unicode block elements
    const BLOCK: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    let lines: Vec<Line> = (0..rows)
        .map(|row| {
            let cell_from_bottom = rows - 1 - row;
            let spans: Vec<Span> = col_db
                .iter()
                .enumerate()
                .map(|(col, &db)| {
                    let norm =
                        ((db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0);
                    let bar_f = norm * rows as f32;
                    let bar_cells = bar_f as usize;
                    let frac = bar_f.fract();

                    let (ch, lit) = if cell_from_bottom < bar_cells {
                        ('█', true)
                    } else if cell_from_bottom == bar_cells && frac > 0.0 {
                        (BLOCK[((frac * 8.0) as usize).min(7)], true)
                    } else {
                        (' ', false)
                    };

                    if lit {
                        Span::styled(ch.to_string(), Style::default().fg(col_color[col]))
                    } else {
                        Span::raw(" ")
                    }
                })
                .collect();
            Line::from(spans)
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}