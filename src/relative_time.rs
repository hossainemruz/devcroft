//! Relative time labels shared by Home project cards and activity cards.
//!
//! Both surfaces show recency (`Opened 2h ago`, `Updated 3h ago`) from
//! different timestamp shapes (RFC-3339 strings vs unix seconds), so the
//! duration bucketing lives here once. Hand-rolled on purpose: recency
//! needs no date dependency, and lexicographic/arithmetic order stays
//! chronological order.

/// Seconds since the unix epoch, zero on clock failure.
pub(crate) fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// `just now`, `5m ago`, `2h ago`, `3d ago`, `2w ago`, `4mo ago`, `1y ago`.
/// Callers prefix the verb (`Opened`, `Updated`).
pub(crate) fn relative_duration_label(diff_secs: i64) -> String {
    if diff_secs < 60 {
        return "just now".to_owned();
    }
    if diff_secs < 3_600 {
        let minutes = diff_secs / 60;
        return if minutes == 1 {
            "1m ago".to_owned()
        } else {
            format!("{minutes}m ago")
        };
    }
    if diff_secs < 86_400 {
        let hours = diff_secs / 3_600;
        return if hours == 1 {
            "1h ago".to_owned()
        } else {
            format!("{hours}h ago")
        };
    }
    if diff_secs < 7 * 86_400 {
        let days = diff_secs / 86_400;
        return if days == 1 {
            "1d ago".to_owned()
        } else {
            format!("{days}d ago")
        };
    }
    if diff_secs < 30 * 86_400 {
        let weeks = diff_secs / (7 * 86_400);
        return if weeks == 1 {
            "1w ago".to_owned()
        } else {
            format!("{weeks}w ago")
        };
    }
    if diff_secs < 365 * 86_400 {
        let months = diff_secs / (30 * 86_400);
        return if months == 1 {
            "1mo ago".to_owned()
        } else {
            format!("{months}mo ago")
        };
    }
    let years = diff_secs / (365 * 86_400);
    if years == 1 {
        "1y ago".to_owned()
    } else {
        format!("{years}y ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_bucket_without_gaps() {
        for secs in [0, 1, 59, -30] {
            assert_eq!(relative_duration_label(secs), "just now");
        }
        assert_eq!(relative_duration_label(60), "1m ago");
        assert_eq!(relative_duration_label(119), "1m ago");
        assert_eq!(relative_duration_label(3_599), "59m ago");
        assert_eq!(relative_duration_label(3_600), "1h ago");
        assert_eq!(relative_duration_label(86_399), "23h ago");
        assert_eq!(relative_duration_label(86_400), "1d ago");
        assert_eq!(relative_duration_label(6 * 86_400), "6d ago");
        assert_eq!(relative_duration_label(7 * 86_400), "1w ago");
        assert_eq!(relative_duration_label(29 * 86_400), "4w ago");
        assert_eq!(relative_duration_label(30 * 86_400), "1mo ago");
        assert_eq!(relative_duration_label(364 * 86_400), "12mo ago");
        assert_eq!(relative_duration_label(365 * 86_400), "1y ago");
        assert_eq!(relative_duration_label(3 * 365 * 86_400), "3y ago");
    }
}
