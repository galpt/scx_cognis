// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// TUI Dashboard — built with ratatui.
//
// Renders six panels:
//   1. Header (scheduler name, live CPU/queued/slice counts).
//   2. System overview (running/queued tasks, CPUs, dispatch/congestion stats).
//   3. Task classification breakdown (interactive/compute/io/rt gauges).
//   4. AI policy state (Q-learning reward EMA, slice, inference latency).
//   5. Scheduling latency chart (rolling 120-sample line chart).
//   6. Trust "Wall of Shame" — flagged/quarantined processes.
//
// All history buffers use HistoryRing — a fixed-size circular array that
// never reallocates after init (zero-alloc after DashboardState creation).

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, List, ListItem, Paragraph},
    Frame, Terminal,
};

use crate::ai::SHAME_MAX;
use crate::stats::Metrics;

// ── HistoryRing ────────────────────────────────────────────────────────────

/// Fixed-size circular ring buffer for f64 time-series data.
///
/// Replaces `VecDeque<f64>` in DashboardState: the ring is allocated once
/// at `DashboardState::default()` and never re-allocates thereafter.
#[derive(Debug, Clone)]
pub struct HistoryRing {
    buf: [f64; HISTORY_LEN],
    head: usize,
    len: usize,
}

impl HistoryRing {
    pub const fn new() -> Self {
        Self {
            buf: [0.0; HISTORY_LEN],
            head: 0,
            len: 0,
        }
    }

    /// Append a value, overwriting the oldest when full.
    pub fn push(&mut self, v: f64) {
        self.buf[self.head] = v;
        self.head = (self.head + 1) % HISTORY_LEN;
        if self.len < HISTORY_LEN {
            self.len += 1;
        }
    }

    /// Iterate values in chronological order (oldest first).
    pub fn iter_ordered(&self) -> impl Iterator<Item = f64> + '_ {
        let start = if self.len < HISTORY_LEN { 0 } else { self.head };
        (0..self.len).map(move |i| self.buf[(start + i) % HISTORY_LEN])
    }

    /// Maximum value in the ring, defaulting to `default` if empty.
    pub fn max_or(&self, default: f64) -> f64 {
        self.iter_ordered().fold(default, f64::max)
    }
}

impl Default for HistoryRing {
    fn default() -> Self {
        Self::new()
    }
}

// ── Dashboard State ────────────────────────────────────────────────────────

const HISTORY_LEN: usize = 120; // ~2 minutes at 1 Hz

#[derive(Debug, Default, Clone, Copy)]
pub struct WallEntry {
    pub pid: i32,
    pub comm: [u8; 16],
    pub trust: f64,
    pub is_flagged: bool,
}

impl WallEntry {
    pub const ZERO: Self = Self {
        pid: 0,
        comm: [0; 16],
        trust: 0.0,
        is_flagged: false,
    };

    pub fn comm_str(&self) -> &str {
        let end = self.comm.iter().position(|&b| b == 0).unwrap_or(16);
        std::str::from_utf8(&self.comm[..end]).unwrap_or("?")
    }
}

/// All mutable state the dashboard needs to render.
#[derive(Debug)]
pub struct DashboardState {
    pub metrics: Metrics,
    pub inference_us: f64, // Most recent inference latency (µs)
    pub inference_hist: HistoryRing,
    pub reward_hist: HistoryRing,
    pub throughput_hist: HistoryRing,
    pub wall_of_shame: [WallEntry; SHAME_MAX],
    pub wall_len: usize,
    /// AI slice history stored as u64 microseconds in a fixed ring buffer.
    pub ai_slice_hist: [u64; HISTORY_LEN],
    pub ai_slice_head: usize,
    pub ai_slice_len: usize,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self {
            metrics: Metrics::default(),
            inference_us: 0.0,
            inference_hist: HistoryRing::new(),
            reward_hist: HistoryRing::new(),
            throughput_hist: HistoryRing::new(),
            wall_of_shame: [WallEntry::ZERO; SHAME_MAX],
            wall_len: 0,
            ai_slice_hist: [0u64; HISTORY_LEN],
            ai_slice_head: 0,
            ai_slice_len: 0,
        }
    }
}

impl DashboardState {
    pub fn push_history(&mut self) {
        self.inference_hist.push(self.inference_us);
        self.reward_hist
            .push(self.metrics.reward_ema_x100 as f64 / 100.0);
        self.throughput_hist
            .push(self.metrics.nr_user_dispatches as f64);
        // Push AI slice into the fixed u64 ring.
        self.ai_slice_hist[self.ai_slice_head] = self.metrics.ai_slice_us;
        self.ai_slice_head = (self.ai_slice_head + 1) % HISTORY_LEN;
        if self.ai_slice_len < HISTORY_LEN {
            self.ai_slice_len += 1;
        }
    }

    pub fn set_wall_of_shame(&mut self, entries: &[WallEntry; SHAME_MAX], len: usize) {
        self.wall_of_shame = *entries;
        self.wall_len = len.min(SHAME_MAX);
    }
}

// ── Terminal setup / teardown ──────────────────────────────────────────────

pub type Term = Terminal<CrosstermBackend<io::Stdout>>;

pub fn setup_terminal() -> Result<Term, io::Error> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

pub fn restore_terminal(term: &mut Term) -> Result<(), io::Error> {
    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    term.show_cursor()?;
    Ok(())
}

// ── Rendering ─────────────────────────────────────────────────────────────

/// Draw one frame.
pub fn draw(frame: &mut Frame, state: &DashboardState) {
    let area = frame.size();

    // Top-level split: header + body.
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    draw_header(frame, root[0], &state.metrics);

    // Body: left column + right column.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(root[1]);

    // Left column: overview + classification + AI policy.
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Min(0),
        ])
        .split(body[0]);

    draw_overview(frame, left[0], &state.metrics);
    draw_classification(frame, left[1], &state.metrics);
    draw_ai_policy(frame, left[2], state);

    // Right column: inference latency chart + wall of shame.
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body[1]);

    draw_latency_chart(frame, right[0], state);
    draw_wall_of_shame(frame, right[1], &state.wall_of_shame[..state.wall_len]);
}

fn draw_header(f: &mut Frame, area: Rect, m: &Metrics) {
    let text = Line::from(vec![
        Span::styled(
            " scx_cognis ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("│ An Attempt at an Intelligent CPU Scheduler │ "),
        Span::styled(
            format!(
                "CPUs: {}  Running: {}  Queued: {}  Slice: {}µs",
                m.nr_cpus, m.nr_running, m.nr_queued, m.ai_slice_us
            ),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  [ press 'q' to quit ]"),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let para = Paragraph::new(text).block(block);
    f.render_widget(para, area);
}

fn draw_overview(f: &mut Frame, area: Rect, m: &Metrics) {
    let load_pct = if m.nr_cpus > 0 {
        (m.nr_running * 100 / m.nr_cpus).min(100)
    } else {
        0
    };

    let items = [
        Line::from(format!(
            "  Dispatched (user/kernel/fail):  {} / {} / {}",
            m.nr_user_dispatches, m.nr_kernel_dispatches, m.nr_failed_dispatches
        )),
        Line::from(format!(
            "  Bounced / Cancelled:            {} / {}",
            m.nr_bounce_dispatches, m.nr_cancel_dispatches
        )),
        Line::from(format!(
            "  Congestion Events:              {}",
            m.nr_sched_congested
        )),
        Line::from(format!(
            "  Page Faults (scheduler):        {}",
            m.nr_page_faults
        )),
        Line::from(format!("  CPU Load:                       {}%", load_pct)),
    ];
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " Overview ",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    let para = Paragraph::new(Vec::from(items)).block(block);
    f.render_widget(para, area);
}

fn draw_classification(f: &mut Frame, area: Rect, m: &Metrics) {
    let total = (m.nr_interactive + m.nr_compute + m.nr_iowait + m.nr_realtime).max(1);

    let items = [
        gauge_line("Interactive", m.nr_interactive, total, Color::Green),
        gauge_line("Compute    ", m.nr_compute, total, Color::Red),
        gauge_line("I/O Wait   ", m.nr_iowait, total, Color::Blue),
        gauge_line("RealTime   ", m.nr_realtime, total, Color::Magenta),
        Line::from(format!(
            "  Quarantined PIDs:   {}   │  Flagged PIDs:  {}",
            m.nr_quarantined, m.nr_flagged
        )),
    ];
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " Task Classification (Heuristic) ",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    let para = Paragraph::new(Vec::from(items)).block(block);
    f.render_widget(para, area);
}

fn gauge_line(label: &str, n: u64, total: u64, color: Color) -> Line<'static> {
    let pct = (n * 100 / total) as usize;
    let bar = "█".repeat(pct / 5).to_owned() + &"░".repeat(20 - pct / 5);
    Line::from(vec![
        Span::raw(format!("  {label}: ")),
        Span::styled(bar, Style::default().fg(color)),
        Span::raw(format!(" {n:>4} ({pct:>3}%)")),
    ])
}

fn draw_ai_policy(f: &mut Frame, area: Rect, state: &DashboardState) {
    let reward = state.metrics.reward_ema_x100 as f64 / 100.0;
    let reward_color = if reward > 0.0 {
        Color::Green
    } else {
        Color::Red
    };

    let items = [
        Line::from(vec![
            Span::raw("  Q-learning Reward: "),
            Span::styled(format!("{:+.4}", reward), Style::default().fg(reward_color)),
        ]),
        Line::from(format!(
            "  Q-learning Slice: {}µs  (base × policy factor)",
            state.metrics.ai_slice_us
        )),
        Line::from(format!("  Inference:        {:.2}µs", state.inference_us)),
        Line::from(vec![
            Span::raw("  Latency Budget:   "),
            Span::styled(
                if state.inference_us < 10.0 {
                    "✓ < 10µs  OK"
                } else {
                    "✗ OVER BUDGET"
                },
                Style::default().fg(if state.inference_us < 10.0 {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
        ]),
    ];
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " Q-learning Policy ",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    let para = Paragraph::new(Vec::from(items)).block(block);
    f.render_widget(para, area);
}

fn draw_latency_chart(f: &mut Frame, area: Rect, state: &DashboardState) {
    let mut data = [(0.0f64, 0.0f64); HISTORY_LEN];
    let mut data_len = 0usize;
    for (i, v) in state.inference_hist.iter_ordered().enumerate() {
        data[data_len] = (i as f64, v);
        data_len += 1;
    }

    let max_y = state.inference_hist.max_or(10.0);
    let max_x = data_len.max(1) as f64;
    let data = &data[..data_len];

    let datasets = vec![Dataset::default()
        .name("Inference µs")
        .marker(symbols::Marker::Dot)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan))
        .data(data)];

    let chart = Chart::new(datasets)
        .block(
            Block::default().borders(Borders::ALL).title(Span::styled(
                " Inference Latency (µs) ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, max_x]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .labels(vec![
                    Span::raw("0"),
                    Span::styled("10µs", Style::default().fg(Color::Green)),
                    Span::raw(format!("{:.0}", max_y)),
                ])
                .bounds([0.0, max_y.max(15.0)]),
        );

    f.render_widget(chart, area);
}

fn draw_wall_of_shame(f: &mut Frame, area: Rect, entries: &[WallEntry]) {
    let header = ListItem::new(Line::from(vec![Span::styled(
        " PID     COMM                TRUST   FLAG",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )]));

    let items =
        std::iter::once(header).chain(entries.iter().take(area.height as usize - 4).map(|e| {
            let flag = if e.is_flagged {
                " ⚠ CHEAT"
            } else {
                "        "
            };
            let color = if e.trust < 0.2 {
                Color::Red
            } else {
                Color::Yellow
            };
            ListItem::new(Line::from(vec![Span::styled(
                format!(" {:<7} {:<20} {:.2} {}", e.pid, e.comm_str(), e.trust, flag),
                Style::default().fg(color),
            )]))
        }));

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(Span::styled(
        " Trust Wall of Shame ",
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )));
    f.render_widget(list, area);
}

// ── Main TUI run loop ─────────────────────────────────────────────────────

/// Shared state handed from the scheduler thread to the TUI thread.
pub type SharedState = Arc<Mutex<DashboardState>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(Mutex::new(DashboardState::default()))
}

/// Run the TUI in a blocking loop until the user presses 'q' or the
/// `shutdown` flag is set.
///
/// Kept for reference — the scheduler now uses [`tick_tui`] to drive the
/// TUI inline from its main loop instead of spawning a separate thread.
#[allow(dead_code)]
pub fn run_tui(state: SharedState, shutdown: Arc<std::sync::atomic::AtomicBool>) {
    let mut terminal = match setup_terminal() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("TUI init failed: {e}");
            return;
        }
    };

    let tick = Duration::from_millis(500);
    let mut last_tick = Instant::now();

    loop {
        // Poll for key events without blocking the redraw loop.
        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc {
                    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
        }

        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        if last_tick.elapsed() >= tick {
            last_tick = Instant::now();
            // Update history inside the lock, then release before drawing.
            if let Ok(mut s) = state.lock() {
                s.push_history();
            }
        }

        // Draw frame.
        let snap = match state.lock() {
            Ok(s) => s.clone(),
            Err(_) => continue,
        };
        // Clone DashboardState so we don't hold the lock during rendering.
        let _ = terminal.draw(|f| draw(f, &snap));
    }

    let _ = restore_terminal(&mut terminal);
}

/// Render one TUI frame and check for quit key.  Call this from the
/// scheduler's main loop to drive the TUI without spawning a thread.
///
/// * `last_hist` — caller-owned `Instant` that governs history push (500 ms).
/// * Returns `true` if the user pressed 'q' or Esc (scheduler should stop).
pub fn tick_tui(state: &SharedState, terminal: &mut Term, last_hist: &mut Instant) -> bool {
    use crossterm::event::{self, Event, KeyCode};

    // Drain all pending events so a queued 'q' is never skipped because
    // an earlier mouse-move, resize, or key-repeat event consumed the slot.
    while event::poll(Duration::from_millis(0)).unwrap_or(false) {
        if let Ok(Event::Key(key)) = event::read() {
            if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc {
                return true;
            }
        }
    }

    // Push history every 500 ms.
    if last_hist.elapsed() >= Duration::from_millis(500) {
        *last_hist = Instant::now();
        if let Ok(mut s) = state.lock() {
            s.push_history();
        }
    }

    // Draw frame.
    if let Ok(snap) = state.lock() {
        let snap = snap.clone();
        let _ = terminal.draw(|f| draw(f, &snap));
    }

    false
}

// DashboardState needs to be Clone for the above to work.
impl Clone for DashboardState {
    fn clone(&self) -> Self {
        Self {
            metrics: self.metrics.clone(),
            inference_us: self.inference_us,
            inference_hist: self.inference_hist.clone(),
            reward_hist: self.reward_hist.clone(),
            throughput_hist: self.throughput_hist.clone(),
            wall_of_shame: self.wall_of_shame,
            wall_len: self.wall_len,
            ai_slice_hist: self.ai_slice_hist,
            ai_slice_head: self.ai_slice_head,
            ai_slice_len: self.ai_slice_len,
        }
    }
}
