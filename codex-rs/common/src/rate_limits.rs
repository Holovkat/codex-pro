#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitWindowKind {
    Hours(u64),
    Weekly,
    Monthly,
    Annual,
}

impl RateLimitWindowKind {
    pub fn from_minutes(minutes: u64) -> Self {
        const MINUTES_PER_HOUR: u64 = 60;
        const MINUTES_PER_DAY: u64 = 24 * MINUTES_PER_HOUR;
        const MINUTES_PER_WEEK: u64 = 7 * MINUTES_PER_DAY;
        const MINUTES_PER_MONTH: u64 = 30 * MINUTES_PER_DAY;
        const ROUNDING_BIAS_MINUTES: u64 = 3;

        if minutes <= MINUTES_PER_DAY.saturating_add(ROUNDING_BIAS_MINUTES) {
            let adjusted = minutes.saturating_add(ROUNDING_BIAS_MINUTES);
            let hours = adjusted / MINUTES_PER_HOUR;
            let clamped = hours.max(1);
            RateLimitWindowKind::Hours(clamped)
        } else if minutes <= MINUTES_PER_WEEK.saturating_add(ROUNDING_BIAS_MINUTES) {
            RateLimitWindowKind::Weekly
        } else if minutes <= MINUTES_PER_MONTH.saturating_add(ROUNDING_BIAS_MINUTES) {
            RateLimitWindowKind::Monthly
        } else {
            RateLimitWindowKind::Annual
        }
    }

    pub fn label(self) -> String {
        match self {
            RateLimitWindowKind::Hours(hours) => format!("{hours}h"),
            RateLimitWindowKind::Weekly => "weekly".to_string(),
            RateLimitWindowKind::Monthly => "monthly".to_string(),
            RateLimitWindowKind::Annual => "annual".to_string(),
        }
    }

    pub fn title(self) -> String {
        match self {
            RateLimitWindowKind::Hours(hours) => format!("{hours}h"),
            RateLimitWindowKind::Weekly => "Weekly".to_string(),
            RateLimitWindowKind::Monthly => "Monthly".to_string(),
            RateLimitWindowKind::Annual => "Annual".to_string(),
        }
    }

    pub fn short_label(self) -> String {
        match self {
            RateLimitWindowKind::Hours(hours) => format!("{hours}h"),
            RateLimitWindowKind::Weekly => "Wk".to_string(),
            RateLimitWindowKind::Monthly => "Mo".to_string(),
            RateLimitWindowKind::Annual => "Yr".to_string(),
        }
    }
}

pub fn classify_window_minutes(minutes: u64) -> RateLimitWindowKind {
    RateLimitWindowKind::from_minutes(minutes)
}

pub fn resolve_window_kind(
    window_minutes: Option<u64>,
    fallback: RateLimitWindowKind,
) -> RateLimitWindowKind {
    match window_minutes {
        Some(minutes) if minutes > 0 => RateLimitWindowKind::from_minutes(minutes),
        _ => fallback,
    }
}

pub fn window_label(window_minutes: Option<u64>, fallback: RateLimitWindowKind) -> String {
    resolve_window_kind(window_minutes, fallback).label()
}

pub fn window_title(window_minutes: Option<u64>, fallback: RateLimitWindowKind) -> String {
    resolve_window_kind(window_minutes, fallback).title()
}

pub fn window_short_label(window_minutes: Option<u64>, fallback: RateLimitWindowKind) -> String {
    resolve_window_kind(window_minutes, fallback).short_label()
}
