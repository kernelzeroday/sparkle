//! Compact labels for token counts, byte sizes and context usage.

/// Rough token count for text of this size, at about four bytes per token.
pub fn estimate_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

/// One decimal below ten and none above, so labels stay short: `1.5k`, `83k`.
fn scaled(value: f64, unit: &str) -> String {
    let text = if value < 10.0 { format!("{value:.1}") } else { format!("{value:.0}") };
    format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
}

/// Shortens a count: `950`, `1.5k`, `197k`, `1.2M`.
pub fn compact_count(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        // Anything higher would round up to `1000k`.
        1_000..999_500 => scaled(count as f64 / 1e3, "k"),
        _ => scaled(count as f64 / 1e6, "M"),
    }
}

/// Shortens a byte size: `512 B`, `1.5 KB`, `813 KB`, `1.2 MB`.
pub fn format_bytes(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    let size = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if size < 1023.5 * KIB {
        scaled(size / KIB, " KB")
    } else {
        scaled(size / (KIB * KIB), " MB")
    }
}

/// Describes text by its estimated tokens and its size: `~197k tok (813 KB)`.
pub fn size_label(bytes: usize) -> String {
    format!("~{} tok ({})", compact_count(estimate_tokens(bytes)), format_bytes(bytes))
}

/// Describes how full the context window is: `197k/258k (76%)`, with `~` for an estimate.
pub fn usage_label(used: u64, window: u64, estimated: bool) -> String {
    let percent = u128::from(used) * 100 / u128::from(window.max(1));
    let mark = if estimated && used > 0 { "~" } else { "" };
    format!("{mark}{}/{} ({percent}%)", compact_count(used), compact_count(window))
}

/// How full the context window may get before sparkle warns, unless configured otherwise.
pub const DEFAULT_WARN_PERCENT: u8 = 80;

/// How a request of some size sits against the model's context window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Fits,
    Near,
    Over,
}

/// Compares a token count with the window. `warn_percent` is where `Near` begins.
pub fn fit(used: u64, window: u64, warn_percent: u8) -> Fit {
    let (used, window) = (u128::from(used), u128::from(window.max(1)));
    if used > window {
        Fit::Over
    } else if used * 100 >= window * u128::from(warn_percent) {
        Fit::Near
    } else {
        Fit::Fits
    }
}

/// The percentage of the window a token count takes up.
pub fn percent_of(used: u64, window: u64) -> u64 {
    (u128::from(used) * 100 / u128::from(window.max(1))) as u64
}

/// A sentence about a request that is close to or beyond the context window.
pub fn warning(used: u64, window: u64, warn_percent: u8) -> Option<(Fit, String)> {
    let state = fit(used, window, warn_percent);
    let lead = match state {
        Fit::Fits => return None,
        Fit::Near => "Close to the context limit",
        Fit::Over => "Over the context limit",
    };
    let (size, limit) = (compact_count(used), compact_count(window));
    let percent = percent_of(used, window);
    Some((state, format!("{lead}: ~{size} of {limit} tokens ({percent}%)")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_changes_at_the_threshold_and_at_the_window() {
        assert_eq!(fit(799, 1000, 80), Fit::Fits);
        assert_eq!(fit(800, 1000, 80), Fit::Near);
        assert_eq!(fit(1000, 1000, 80), Fit::Near);
        assert_eq!(fit(1001, 1000, 80), Fit::Over);
        assert_eq!(fit(500, 1000, 50), Fit::Near);
        assert_eq!(fit(2, 0, 80), Fit::Over);
    }

    #[test]
    fn warnings_describe_the_size_and_stay_quiet_when_it_fits() {
        assert_eq!(warning(100, 1000, 80), None);
        let (state, text) = warning(52_000, 64_000, 80).unwrap();
        assert_eq!(state, Fit::Near);
        assert_eq!(text, "Close to the context limit: ~52k of 64k tokens (81%)");
        let (state, text) = warning(120_000, 64_000, 80).unwrap();
        assert_eq!(state, Fit::Over);
        assert_eq!(text, "Over the context limit: ~120k of 64k tokens (187%)");
    }

    #[test]
    fn counts_stay_short_across_unit_boundaries() {
        let cases = [
            (0, "0"),
            (999, "999"),
            (1_000, "1k"),
            (1_500, "1.5k"),
            (9_960, "10k"),
            (83_000, "83k"),
            (197_000, "197k"),
            (999_499, "999k"),
            (999_500, "1M"),
            (1_200_000, "1.2M"),
        ];
        for (count, expected) in cases {
            assert_eq!(compact_count(count), expected);
        }
    }

    #[test]
    fn byte_sizes_use_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(813 * 1024), "813 KB");
        assert_eq!(format_bytes(1024 * 1024 - 1), "1 MB");
    }

    #[test]
    fn size_labels_show_tokens_and_bytes() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(5), 2);
        assert_eq!(size_label(788_000), "~197k tok (770 KB)");
    }

    #[test]
    fn usage_labels_mark_estimates() {
        assert_eq!(usage_label(197_000, 258_000, false), "197k/258k (76%)");
        assert_eq!(usage_label(197_000, 258_000, true), "~197k/258k (76%)");
        assert_eq!(usage_label(0, 258_000, true), "0/258k (0%)");
        assert_eq!(usage_label(300, 200, false), "300/200 (150%)");
    }
}
