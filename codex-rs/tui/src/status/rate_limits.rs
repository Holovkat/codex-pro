use super::helpers::format_reset_timestamp;
use crate::chatwidget::get_limits_duration;
use chrono::DateTime;
use chrono::Local;
use codex_common::rate_limits::RateLimitWindowKind;
use codex_common::rate_limits::resolve_window_kind;
use codex_core::protocol::RateLimitSnapshot;
use codex_core::protocol::RateLimitWindow;

const STATUS_LIMIT_BAR_SEGMENTS: usize = 20;
const STATUS_LIMIT_BAR_FILLED: &str = "█";
const STATUS_LIMIT_BAR_EMPTY: &str = "░";

#[derive(Debug, Clone)]
pub(crate) struct StatusRateLimitRow {
    pub label: String,
    pub percent_used: f64,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum StatusRateLimitData {
    Available(Vec<StatusRateLimitRow>),
    Missing,
}

#[derive(Debug, Clone)]
pub(crate) struct RateLimitWindowDisplay {
    pub used_percent: f64,
    pub resets_at: Option<String>,
    pub window_minutes: Option<i64>,
    fallback_kind: RateLimitWindowKind,
}

impl RateLimitWindowDisplay {
    fn from_window(
        window: &RateLimitWindow,
        captured_at: DateTime<Local>,
        fallback_kind: RateLimitWindowKind,
    ) -> Self {
        let resets_at = window
            .resets_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|dt| dt.with_timezone(&Local))
            .map(|dt| format_reset_timestamp(dt, captured_at));

        Self {
            used_percent: window.used_percent,
            resets_at,
            window_minutes: window.window_minutes,
            fallback_kind,
        }
    }

    fn window_kind(&self) -> RateLimitWindowKind {
        resolve_window_kind(self.window_minutes, self.fallback_kind)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RateLimitSnapshotDisplay {
    pub primary: Option<RateLimitWindowDisplay>,
    pub secondary: Option<RateLimitWindowDisplay>,
}

pub(crate) fn rate_limit_snapshot_display(
    snapshot: &RateLimitSnapshot,
    captured_at: DateTime<Local>,
) -> RateLimitSnapshotDisplay {
    RateLimitSnapshotDisplay {
        primary: snapshot.primary.as_ref().map(|window| {
            RateLimitWindowDisplay::from_window(window, captured_at, RateLimitWindowKind::Hours(5))
        }),
        secondary: snapshot.secondary.as_ref().map(|window| {
            RateLimitWindowDisplay::from_window(window, captured_at, RateLimitWindowKind::Weekly)
        }),
    }
}

pub(crate) fn compose_rate_limit_data(
    snapshot: Option<&RateLimitSnapshotDisplay>,
) -> StatusRateLimitData {
    match snapshot {
        Some(snapshot) => {
            let mut rows = Vec::with_capacity(2);

            if let Some(primary) = snapshot.primary.as_ref() {
                rows.push(StatusRateLimitRow {
                    label: format!("{} limit", primary.window_kind().title()),
                    percent_used: primary.used_percent,
                    resets_at: primary.resets_at.clone(),
                });
            }

            if let Some(secondary) = snapshot.secondary.as_ref() {
                rows.push(StatusRateLimitRow {
                    label: format!("{} limit", secondary.window_kind().title()),
                    percent_used: secondary.used_percent,
                    resets_at: secondary.resets_at.clone(),
                });
            }

            if rows.is_empty() {
                StatusRateLimitData::Available(vec![])
            } else {
                StatusRateLimitData::Available(rows)
            }
        }
        None => StatusRateLimitData::Missing,
    }
}

pub(crate) fn compose_rate_limit_footer(
    snapshot: Option<&RateLimitSnapshotDisplay>,
) -> Vec<String> {
    let mut summaries = Vec::new();

    if let Some(display) = snapshot {
        if let Some(primary) = display.primary.as_ref()
            && let Some(summary) = footer_summary(primary)
        {
            summaries.push(summary);
        }
        if let Some(secondary) = display.secondary.as_ref()
            && let Some(summary) = footer_summary(secondary)
        {
            summaries.push(summary);
        }
    }

    summaries
}

fn footer_summary(window: &RateLimitWindowDisplay) -> Option<String> {
    let label = window
        .window_minutes
        .map(get_limits_duration)
        .unwrap_or_else(|| window.window_kind().short_label());
    let percent = format!("{:.0}%", window.used_percent);
    let mut summary = format!("{percent} {label}");

    if let Some(reset) = window.resets_at.as_deref() {
        let cleaned = reset.replace(" on ", " ");
        if !cleaned.is_empty() {
            summary.push(' ');
            summary.push_str(cleaned.trim());
        }
    }

    Some(summary)
}

pub(crate) fn render_status_limit_progress_bar(percent_used: f64) -> String {
    let ratio = (percent_used / 100.0).clamp(0.0, 1.0);
    let filled = (ratio * STATUS_LIMIT_BAR_SEGMENTS as f64).round() as usize;
    let filled = filled.min(STATUS_LIMIT_BAR_SEGMENTS);
    let empty = STATUS_LIMIT_BAR_SEGMENTS.saturating_sub(filled);
    format!(
        "[{}{}]",
        STATUS_LIMIT_BAR_FILLED.repeat(filled),
        STATUS_LIMIT_BAR_EMPTY.repeat(empty)
    )
}

pub(crate) fn format_status_limit_summary(percent_used: f64) -> String {
    format!("{percent_used:.0}% used")
}
