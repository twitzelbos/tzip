use indicatif::{ProgressBar, ProgressStyle};

/// Progress renderer that supports two modes:
///
/// * `total > 0` → deterministic gauge with bytes/total, MB/s and ETA.
/// * `total == 0` → indeterminate spinner + running counter (used in
///   streaming-walk mode, where we don't yet know the total).
///
/// The mode is chosen at construction. Callers can push updates via `inc`
/// and `set_msg` regardless of mode.
pub struct Progress {
    pub bar: Option<ProgressBar>,
    determinate: bool,
}

impl Progress {
    pub fn new(total_bytes: u64, total_files: u64, quiet: bool) -> Self {
        if quiet {
            return Self { bar: None, determinate: true };
        }
        if total_bytes > 0 {
            let style = ProgressStyle::with_template(
                "{spinner:.cyan} [{elapsed_precise}] {bar:40.cyan/blue} {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta}) {msg}",
            )
            .unwrap()
            .progress_chars("=> ");
            let bar = ProgressBar::new(total_bytes);
            bar.set_style(style);
            bar.set_message(format!("0/{} files", total_files));
            Self { bar: Some(bar), determinate: true }
        } else {
            // Unknown total — spinner + running byte counter.
            let style = ProgressStyle::with_template(
                "{spinner:.cyan} [{elapsed_precise}] {bytes} ({bytes_per_sec}) {msg}",
            )
            .unwrap();
            let bar = ProgressBar::new_spinner();
            bar.set_style(style);
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            bar.set_message("scanning…");
            Self { bar: Some(bar), determinate: false }
        }
    }

    pub fn inc(&self, bytes: u64) {
        if let Some(b) = &self.bar {
            if self.determinate {
                b.inc(bytes);
            } else {
                // In spinner mode `inc` moves the position (which the template
                // renders as `{bytes}` via ByteFormatter). Works for both.
                b.inc(bytes);
            }
        }
    }

    pub fn set_msg(&self, msg: String) {
        if let Some(b) = &self.bar {
            b.set_message(msg);
        }
    }

    pub fn finish(&self, msg: &str) {
        if let Some(b) = &self.bar {
            b.finish_with_message(msg.to_string());
        }
    }
}
