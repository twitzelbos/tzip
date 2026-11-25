//! Ratatui dashboard.
//!
//! Enabled with `--tui`. Runs a render thread that receives events via a
//! `crossbeam-channel` from the pipeline (main + workers). Terminal is put
//! into raw mode with the alternate screen; we restore on drop / finish.
//!
//! Layout:
//!
//!     ┌──────────────────────── tzip ─────────────────────────┐
//!     │ overall bar   |   bytes / total   |   MB/s   |   ETA  │
//!     │ throughput ▁▂▄█▇▅▄▃▂▂▁▁  peak N MB/s  workers X/Y     │
//!     ├───────────────────────────────────────────────────────┤
//!     │ worker 0: <file>          <MB/s>                      │
//!     │ worker 1: <file>          <MB/s>                      │
//!     │ ...                                                   │
//!     └───────────────────────────────────────────────────────┘
//!
//! `q` cancels (sends SIGINT-equivalent — currently only sets a flag; the
//! pipeline honors it on the next iteration).

use crossbeam_channel::{unbounded, Sender};
use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Sparkline};
use ratatui::Terminal;
use std::collections::VecDeque;
use std::io::stdout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

enum Ev {
    WorkerStarted { id: usize, file: String, size: u64 },
    WorkerFinished { id: usize, mb_per_s: f64 },
    Finish,
}

#[derive(Clone)]
pub struct Handle {
    tx: Sender<Ev>,
    canceled: Arc<AtomicBool>,
    join: Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
}

impl Handle {
    pub fn worker_started(&self, id: usize, file: String, size: u64) {
        let _ = self.tx.send(Ev::WorkerStarted { id, file, size });
    }
    pub fn worker_finished(&self, id: usize, mb_per_s: f64) {
        let _ = self.tx.send(Ev::WorkerFinished { id, mb_per_s });
    }
    pub fn finish(&self) {
        let _ = self.tx.send(Ev::Finish);
        if let Ok(mut g) = self.join.lock() {
            if let Some(j) = g.take() {
                let _ = j.join();
            }
        }
    }
    #[allow(dead_code)]
    pub fn canceled(&self) -> bool {
        self.canceled.load(Ordering::Relaxed)
    }
}

pub fn start(total_bytes: u64, total_files: u64, cpu_jobs: usize) -> Handle {
    let (tx, rx) = unbounded::<Ev>();
    let canceled = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&canceled);

    let join = thread::spawn(move || {
        if let Err(e) = run_ui(rx, cancel_flag, total_bytes, total_files, cpu_jobs) {
            eprintln!("tui error: {e}");
        }
    });

    Handle {
        tx,
        canceled,
        join: Arc::new(std::sync::Mutex::new(Some(join))),
    }
}

fn run_ui(
    rx: crossbeam_channel::Receiver<Ev>,
    canceled: Arc<AtomicBool>,
    total_bytes: u64,
    total_files: u64,
    cpu_jobs: usize,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    struct WorkerRow {
        file: String,
        size: u64,
        started: Option<Instant>,
        last_mb_per_s: f64,
        files_done: u64,
    }
    let mut workers: Vec<WorkerRow> = (0..cpu_jobs)
        .map(|_| WorkerRow {
            file: String::new(),
            size: 0,
            started: None,
            last_mb_per_s: 0.0,
            files_done: 0,
        })
        .collect();

    let mut done_bytes = 0u64;
    let mut done_files = 0u64;
    let started = Instant::now();
    let mut last_draw = Instant::now() - Duration::from_secs(1);

    // Throughput history: one sample every ~500 ms of aggregate MB/s
    // over that interval. Rolling window of ~30 seconds (60 samples).
    // Rendered as a Sparkline in the header — shows if the pipeline is
    // steady-state, ramping, or stalling.
    const SPARK_CAP: usize = 60;
    let mut throughput_samples: VecDeque<u64> = VecDeque::with_capacity(SPARK_CAP);
    let mut last_sample_at = Instant::now();
    let mut last_sample_bytes = 0u64;
    let mut peak_mbps = 0.0f64;

    loop {
        // Drain events non-blocking
        loop {
            match rx.try_recv() {
                Ok(Ev::WorkerStarted { id, file, size }) => {
                    if let Some(w) = workers.get_mut(id) {
                        w.file = file;
                        w.size = size;
                        w.started = Some(Instant::now());
                    }
                }
                Ok(Ev::WorkerFinished { id, mb_per_s }) => {
                    if let Some(w) = workers.get_mut(id) {
                        w.last_mb_per_s = mb_per_s;
                        w.files_done += 1;
                        done_bytes = done_bytes.saturating_add(w.size);
                        done_files += 1;
                        w.started = None;
                        w.size = 0;
                        w.file.clear();
                    }
                }
                Ok(Ev::Finish) => {
                    cleanup(&mut term)?;
                    return Ok(());
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(_) => {
                    cleanup(&mut term)?;
                    return Ok(());
                }
            }
        }

        // Keyboard poll
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                    canceled.store(true, Ordering::Relaxed);
                }
            }
        }

        // Take a throughput sample every ~500 ms — instantaneous MB/s
        // over the last interval, so the sparkline reflects live rate
        // rather than lifetime average. Skip while last_draw is fresh
        // to keep sampling regular.
        if last_sample_at.elapsed() >= Duration::from_millis(500) {
            let dt = last_sample_at.elapsed().as_secs_f64();
            let dbytes = done_bytes.saturating_sub(last_sample_bytes);
            let inst_mbps = (dbytes as f64 / 1_048_576.0) / dt.max(0.001);
            if inst_mbps > peak_mbps {
                peak_mbps = inst_mbps;
            }
            if throughput_samples.len() >= SPARK_CAP {
                throughput_samples.pop_front();
            }
            throughput_samples.push_back(inst_mbps.round() as u64);
            last_sample_at = Instant::now();
            last_sample_bytes = done_bytes;
        }

        if last_draw.elapsed() < Duration::from_millis(80) {
            continue;
        }
        last_draw = Instant::now();

        term.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(6), Constraint::Min(3)])
                .split(area);

            // Header: outer box, with gauge on top row and sparkline
            // on the row below.
            let header = chunks[0];
            let header_inner = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Length(3)])
                .split(header);

            // Total-known path shows a real percentage + ETA; streaming
            // mode (total_bytes == 0) shows an indeterminate label
            // instead of "0.0 MB / 0.0 MB • ETA 0m00s" nonsense.
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            let mbps = (done_bytes as f64 / 1_048_576.0) / elapsed;
            let known_total = total_bytes > 0;
            let pct = if known_total {
                ((done_bytes.min(total_bytes) as u128 * 100) / total_bytes as u128) as u16
            } else {
                0
            };
            let label = if known_total {
                let remaining_mb = (total_bytes.saturating_sub(done_bytes)) as f64 / 1_048_576.0;
                let eta = if mbps > 0.0 {
                    let secs = (remaining_mb / mbps) as u64;
                    format!("{}m{:02}s", secs / 60, secs % 60)
                } else {
                    "…".into()
                };
                format!(
                    "{:.1} MB / {:.1} MB  •  {} / {} files  •  {:.1} MB/s  •  ETA {}  •  press q to cancel",
                    done_bytes as f64 / 1_048_576.0,
                    total_bytes as f64 / 1_048_576.0,
                    done_files,
                    total_files,
                    mbps,
                    eta,
                )
            } else {
                format!(
                    "{:.1} MB written  •  {} files  •  {:.1} MB/s  •  discovering…  •  press q to cancel",
                    done_bytes as f64 / 1_048_576.0,
                    done_files,
                    mbps,
                )
            };
            let gauge = Gauge::default()
                .block(Block::default().borders(Borders::ALL).title(Span::styled(
                    "tzip",
                    Style::default().add_modifier(Modifier::BOLD),
                )))
                .gauge_style(Style::default().fg(Color::Cyan))
                .percent(pct)
                .label(label);
            f.render_widget(gauge, header_inner[0]);

            // Sparkline row: throughput history + peak + active workers.
            let active = workers.iter().filter(|w| w.started.is_some()).count();
            let spark_data: Vec<u64> = throughput_samples.iter().copied().collect();
            let sparkline_title = format!(
                "throughput  •  peak {:.0} MB/s  •  workers {}/{}",
                peak_mbps,
                active,
                workers.len(),
            );
            let sparkline = Sparkline::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(sparkline_title),
                )
                .data(&spark_data)
                .style(Style::default().fg(Color::Green));
            f.render_widget(sparkline, header_inner[1]);

            let mut lines: Vec<Line> = Vec::with_capacity(workers.len());
            for (i, w) in workers.iter().enumerate() {
                let state = if w.started.is_some() {
                    format!(
                        "▶ {:<48}  {:>6.1} MB/s  ({} done)",
                        truncate(&w.file, 48),
                        w.last_mb_per_s,
                        w.files_done
                    )
                } else {
                    format!(
                        "· idle{}  {:>6.1} MB/s  ({} done)",
                        " ".repeat(43),
                        w.last_mb_per_s,
                        w.files_done
                    )
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("worker {:>2}: ", i),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(state),
                ]));
            }
            let workers_para =
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("workers"));
            f.render_widget(workers_para, chunks[1]);
        })?;

        if canceled.load(Ordering::Relaxed) {
            // Best-effort: keep drawing until pipeline finishes, but the flag
            // is available for future graceful cancel plumbing.
        }
    }

    // unreachable
}

fn cleanup<B: ratatui::backend::Backend + std::io::Write>(
    term: &mut Terminal<B>,
) -> std::io::Result<()> {
    disable_raw_mode()?;
    term.backend_mut().execute(LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().rev().take(n - 1).collect::<Vec<_>>().into_iter().rev().collect();
        out.insert(0, '…');
        out
    }
}
