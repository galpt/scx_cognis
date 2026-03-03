// Copyright (c) scx_cognis contributors
// SPDX-License-Identifier: GPL-2.0-only
//
// TUI Dashboard — built with ratatui.
//
// Renders four panels:
//   1. System overview (running/queued tasks, CPUs, slice).
//   2. Task classification breakdown (interactive/compute/io/rt).
//   3. AI policy state (PPO reward EMA, predicted vs actual burst).
//   4. Reputation "Wall of Shame" — flagged/quarantined processes.

use std::collections::VecDeque;
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
    widgets::{
        Axis, Block, Borders, Chart, Dataset, GraphType,
        List, ListItem, Paragraph,
    },
    Frame, Terminal,
};

use crate::stats::Metrics;

// ── Dashboard State ────────────────────────────────────────────────────────

const HISTORY_LEN: usize = 120; // ~2 minutes at 1 Hz

#[derive(Debug, Default, Clone)]
pub struct WallEntry {
    pub pid:        i32,
    pub comm:       String,
    pub trust:      f64,
    pub is_flagged: bool,
}

/// All mutable state the dashboard needs to render.
#[derive(Debug, Default)]
pub struct DashboardState {
    pub metrics:           Metrics,
    pub inference_us:      f64,     // Most recent inference latency (µs)
    pub inference_hist:    VecDeque<f64>,
    pub reward_hist:       VecDeque<f64>,
    pub throughput_hist:   VecDeque<f64>,
    pub wall_of_shame:     Vec<WallEntry>,
    pub ai_slice_hist:     VecDeque<u64>,
}

impl DashboardState {
    fn push_history(&mut self) {
        push_bounded(&mut self.inference_hist,  self.inference_us, HISTORY_LEN);
        push_bounded(&mut self.reward_hist,     self.metrics.reward_ema_x100 as f64 / 100.0, HISTORY_LEN);
        push_bounded(&mut self.throughput_hist, self.metrics.nr_user_dispatches as f64, HISTORY_LEN);
        push_bounded(&mut self.ai_slice_hist,   self.metrics.ai_slice_us, HISTORY_LEN);
    }
}

fn push_bounded<T>(q: &mut VecDeque<T>, v: T, max: usize) {
    q.push_back(v);
    while q.len() > max {
        q.pop_front();
    }
}

// ── Terminal setup / teardown ──────────────────────────────────────────────

type Term = Terminal<CrosstermBackend<io::Stdout>>;

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

    draw_overview(frame,   left[0], &state.metrics);
    draw_classification(frame, left[1], &state.metrics);
    draw_ai_policy(frame,  left[2], state);

    // Right column: inference latency chart + wall of shame.
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body[1]);

    draw_latency_chart(frame,   right[0], state);
    draw_wall_of_shame(frame, right[1], &state.wall_of_shame);
}

fn draw_header(f: &mut Frame, area: Rect, m: &Metrics) {
    let text = Line::from(vec![
        Span::styled(" scx_cognis ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("│ An Attempt at an Intelligent CPU Scheduler │ "),
        Span::styled(
            format!("CPUs: {}  Running: {}  Queued: {}  Slice: {}µs",
                m.nr_cpus, m.nr_running, m.nr_queued, m.ai_slice_us),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  [ press 'q' to quit ]"),
    ]);
    let block = Block::default().borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let para = Paragraph::new(text).block(block);
    f.render_widget(para, area);
}

fn draw_overview(f: &mut Frame, area: Rect, m: &Metrics) {
    let load_pct = if m.nr_cpus > 0 {
        (m.nr_running * 100 / m.nr_cpus).min(100)
    } else { 0 };

    let items: Vec<Line> = vec![
        Line::from(format!("  Dispatched (user/kernel/fail):  {} / {} / {}",
            m.nr_user_dispatches, m.nr_kernel_dispatches, m.nr_failed_dispatches)),
        Line::from(format!("  Bounced / Cancelled:            {} / {}",
            m.nr_bounce_dispatches, m.nr_cancel_dispatches)),
        Line::from(format!("  Congestion Events:              {}", m.nr_sched_congested)),
        Line::from(format!("  Page Faults (scheduler):        {}", m.nr_page_faults)),
        Line::from(format!("  CPU Load:                       {}%", load_pct)),
    ];
    let block = Block::default().borders(Borders::ALL)
        .title(Span::styled(" Overview ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    let para = Paragraph::new(items).block(block);
    f.render_widget(para, area);
}

fn draw_classification(f: &mut Frame, area: Rect, m: &Metrics) {
    let total = (m.nr_interactive + m.nr_compute + m.nr_iowait + m.nr_realtime).max(1);

    let items: Vec<Line> = vec![
        gauge_line("Interactive", m.nr_interactive, total, Color::Green),
        gauge_line("Compute    ", m.nr_compute,     total, Color::Red),
        gauge_line("I/O Wait   ", m.nr_iowait,      total, Color::Blue),
        gauge_line("RealTime   ", m.nr_realtime,     total, Color::Magenta),
        Line::from(format!("  Quarantined PIDs:   {}   │  Flagged TGIDs:  {}",
            m.nr_quarantined, m.nr_flagged)),
    ];
    let block = Block::default().borders(Borders::ALL)
        .title(Span::styled(" Task Classification (KNN) ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    let para = Paragraph::new(items).block(block);
    f.render_widget(para, area);
}

fn gauge_line(label: &str, n: u64, total: u64, color: Color) -> Line<'static> {
    let pct  = (n * 100 / total) as usize;
    let bar  = "█".repeat(pct / 5).to_owned() + &"░".repeat(20 - pct / 5);
    Line::from(vec![
        Span::raw(format!("  {label}: ")),
        Span::styled(bar, Style::default().fg(color)),
        Span::raw(format!(" {n:>4} ({pct:>3}%)")),
    ])
}

fn draw_ai_policy(f: &mut Frame, area: Rect, state: &DashboardState) {
    let reward = state.metrics.reward_ema_x100 as f64 / 100.0;
    let reward_color = if reward > 0.0 { Color::Green } else { Color::Red };

    let items: Vec<Line> = vec![
        Line::from(vec![
            Span::raw("  PPO Reward EMA:   "),
            Span::styled(format!("{:+.4}", reward), Style::default().fg(reward_color)),
        ]),
        Line::from(format!("  AI Time Slice:    {}µs  (base + PPO adjustment)", state.metrics.ai_slice_us)),
        Line::from(format!("  Inference:        {:.2}µs", state.inference_us)),
        Line::from(vec![
            Span::raw("  Latency Budget:   "),
            Span::styled(
                if state.inference_us < 10.0 { "✓ < 10µs  OK" } else { "✗ OVER BUDGET" },
                Style::default().fg(if state.inference_us < 10.0 { Color::Green } else { Color::Red }),
            ),
        ]),
    ];
    let block = Block::default().borders(Borders::ALL)
        .title(Span::styled(" AI Policy (PPO-lite) ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    let para = Paragraph::new(items).block(block);
    f.render_widget(para, area);
}

fn draw_latency_chart(f: &mut Frame, area: Rect, state: &DashboardState) {
    let data: Vec<(f64, f64)> = state.inference_hist
        .iter()
        .enumerate()
        .map(|(i, &v)| (i as f64, v))
        .collect();

    let max_y = state.inference_hist.iter().cloned().fold(10.0_f64, f64::max);
    let max_x = data.len().max(1) as f64;

    let datasets = vec![
        Dataset::default()
            .name("Inference µs")
            .marker(symbols::Marker::Dot)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Cyan))
            .data(&data),
    ];

    let chart = Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL)
            .title(Span::styled(" Inference Latency (µs) ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))))
        .x_axis(Axis::default()
            .style(Style::default().fg(Color::DarkGray))
            .bounds([0.0, max_x]))
        .y_axis(Axis::default()
            .style(Style::default().fg(Color::DarkGray))
            .labels(vec![
                Span::raw("0"),
                Span::styled("10µs", Style::default().fg(Color::Green)),
                Span::raw(format!("{:.0}", max_y)),
            ])
            .bounds([0.0, max_y.max(15.0)]));

    f.render_widget(chart, area);
}

fn draw_wall_of_shame(f: &mut Frame, area: Rect, entries: &[WallEntry]) {
    let header = ListItem::new(Line::from(vec![
        Span::styled(" PID     COMM                TRUST   FLAG",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
    ]));

    let mut items = vec![header];
    for e in entries.iter().take(area.height as usize - 4) {
        let flag = if e.is_flagged { " ⚠ CHEAT" } else { "        " };
        let color = if e.trust < 0.2 { Color::Red } else { Color::Yellow };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!(" {:<7} {:<20} {:.2} {}", e.pid, e.comm, e.trust, flag),
                Style::default().fg(color),
            ),
        ])));
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL)
            .title(Span::styled(" Reputation Wall of Shame ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))));
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
/// Call this in a separate thread via `std::thread::spawn`.
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

// DashboardState needs to be Clone for the above to work.
impl Clone for DashboardState {
    fn clone(&self) -> Self {
        Self {
            metrics:         self.metrics.clone(),
            inference_us:    self.inference_us,
            inference_hist:  self.inference_hist.clone(),
            reward_hist:     self.reward_hist.clone(),
            throughput_hist: self.throughput_hist.clone(),
            wall_of_shame:   self.wall_of_shame.clone(),
            ai_slice_hist:   self.ai_slice_hist.clone(),
        }
    }
}
