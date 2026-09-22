use std::fmt;

use chrono_tz::Tz;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// Timestamps log lines in a configured IANA timezone instead of the
/// default UTC, using `chrono-tz`'s bundled zone database rather than the
/// system's (the distroless runtime image ships no `/usr/share/zoneinfo`,
/// so a libc-based local-time lookup would silently fall back to UTC
/// anyway).
struct TzTimer(Tz);

impl FormatTime for TzTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        write!(
            w,
            "{}",
            chrono::Utc::now()
                .with_timezone(&self.0)
                .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
        )
    }
}

/// Sets up the global tracing subscriber. Must run before any other
/// `tracing::*!` call.
///
/// `RUST_LOG` (tracing's `EnvFilter` syntax, e.g. `jmap2telegram=debug`)
/// always wins when set — it's the power-user escape hatch. `LOG_LEVEL`
/// is the simpler, documented knob for everyone else (`trace`, `debug`,
/// `info`, `warn`, `error`); it's what shows up in a container's `docker
/// logs`, so container orchestrators/operators don't need to know
/// tracing's filter syntax just to turn logging up or down.
pub fn init(log_level: Option<&str>, timezone: Tz) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = log_level.filter(|s| !s.is_empty()).unwrap_or("info");
        tracing_subscriber::EnvFilter::try_new(level)
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    });

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(TzTimer(timezone))
        .init();
}
