use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

pub const BAR_TEMPLATE: &str =
    "[{bar:20}] {percent}% | Peers: {msg} | D: {have} @ {speed_txt} | T: {full_size} | ETA: {estimate}";

pub fn progress_style() -> ProgressStyle {
    ProgressStyle::with_template(BAR_TEMPLATE)
        .expect("valid progress template")
        .progress_chars("█░")
        .with_key(
            "have",
            |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{}", format_size(state.pos()));
            },
        )
        .with_key(
            "speed_txt",
            |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{}", format_speed(state.per_sec()));
            },
        )
        .with_key(
            "full_size",
            |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{}", format_size(state.len().unwrap_or(0)));
            },
        )
        .with_key(
            "estimate",
            |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{}", format_eta(Some(state.eta())));
            },
        )
}

pub fn new_bar(total: u64) -> ProgressBar {
    let bar = ProgressBar::new(total.max(1));
    bar.set_style(progress_style());
    bar.set_message("0");
    bar
}

pub fn format_eta(eta: Option<Duration>) -> String {
    let Some(eta) = eta else {
        return "N/A".to_string();
    };
    if eta == Duration::MAX {
        return "N/A".to_string();
    }

    let seconds = eta.as_secs();
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    let weeks = days / 7;
    let months = (weeks as f64 / 4.345) as u64;
    let years = months / 12;

    fn plural(n: u64, unit: &str) -> String {
        if n == 1 {
            format!("1 {unit}")
        } else {
            format!("{n} {unit}s")
        }
    }

    if years > 0 {
        plural(years, "year")
    } else if months > 0 {
        plural(months, "month")
    } else if weeks > 0 {
        plural(weeks, "week")
    } else if days > 0 {
        plural(days, "day")
    } else if hours > 0 {
        plural(hours, "hour")
    } else if minutes > 0 {
        plural(minutes, "minute")
    } else {
        plural(seconds, "second")
    }
}

pub fn format_speed(speed: f64) -> String {
    if !speed.is_finite() || speed <= 0.0 {
        return "0.00 B/s".to_string();
    }
    let level = (speed.log(1024.0).floor() as i32).clamp(0, 3);
    match level {
        0 => format!("{speed:.2} B/s"),
        1 => format!("{:.2} KB/s", speed / 1024.0),
        2 => format!("{:.2} MB/s", speed / (1024.0 * 1024.0)),
        _ => format!("{:.2} GB/s", speed / (1024.0 * 1024.0 * 1024.0)),
    }
}

pub fn format_size(size: u64) -> String {
    if size == 0 {
        return "0.00 B".to_string();
    }
    let size = size as f64;
    let level = (size.log(1024.0).floor() as i32).clamp(0, 3);
    match level {
        0 => format!("{size:.2} B"),
        1 => format!("{:.2} KB", size / 1024.0),
        2 => format!("{:.3} MB", size / (1024.0 * 1024.0)),
        _ => format!("{:.4} GB", size / (1024.0 * 1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_zero_and_known_sizes() {
        assert_eq!(format_size(0), "0.00 B");
        assert_eq!(format_size(512), "512.00 B");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1024 * 1024), "1.000 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0000 GB");
    }

    #[test]
    fn formats_speed_and_eta_units() {
        assert_eq!(format_speed(0.0), "0.00 B/s");
        assert_eq!(format_speed(f64::NAN), "0.00 B/s");
        assert_eq!(format_speed(1024.0), "1.00 KB/s");
        assert_eq!(format_eta(None), "N/A");
        assert_eq!(format_eta(Some(Duration::MAX)), "N/A");
        assert_eq!(format_eta(Some(Duration::from_secs(1))), "1 second");
        assert_eq!(format_eta(Some(Duration::from_secs(2))), "2 seconds");
        assert_eq!(format_eta(Some(Duration::from_secs(60))), "1 minute");
        assert_eq!(format_eta(Some(Duration::from_secs(3600))), "1 hour");
        assert_eq!(format_eta(Some(Duration::from_secs(86400))), "1 day");
    }

    #[test]
    fn progress_template_is_valid() {
        let bar = new_bar(100);
        bar.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        bar.set_position(50);
        bar.set_message("3");
        let _ = bar.elapsed();
        bar.finish_and_clear();
    }
}
