//! Local quota observations. A failed or repeated read never invents fresh usage.

use serde::{Deserialize, Serialize};

pub const FRESH_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuotaIssue {
    Authentication,
    RateLimited,
    Network,
    Timeout,
    Upstream,
    InvalidResponse,
    Unsupported,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationSource {
    #[default]
    Api,
    ResponseHeaders,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub id: String,
    pub span_secs: u64,
    pub used_percent: f64,
    pub resets_at: Option<u64>,
    pub observed_at: u64,
    #[serde(default)]
    pub source: ObservationSource,
    // Ordering is process-local. Restored observations precede a new read.
    #[serde(skip)]
    pub sequence: u64,
}

impl QuotaWindow {
    pub fn new(
        span_secs: u64,
        used_percent: f64,
        resets_at: Option<u64>,
        observed_at: u64,
        sequence: u64,
        source: ObservationSource,
    ) -> Option<Self> {
        if span_secs == 0 || !used_percent.is_finite() {
            return None;
        }
        let id = if span_secs.is_multiple_of(86_400) {
            format!("{}d", span_secs / 86_400)
        } else if span_secs.is_multiple_of(3600) {
            format!("{}h", span_secs / 3600)
        } else {
            format!("{span_secs}s")
        };
        Some(Self {
            id,
            span_secs,
            used_percent: used_percent.clamp(0.0, 100.0),
            resets_at,
            observed_at,
            sequence,
            source,
        })
    }

    pub fn fresh(&self, now: u64) -> bool {
        self.used_percent.is_finite()
            && self.span_secs > 0
            && self.observed_at <= now
            && now - self.observed_at < FRESH_SECS
            && self.resets_at.is_none_or(|at| at > now)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaState {
    /// Opaque local identity tag; management views deliberately omit it.
    pub subject: String,
    pub observed_at: Option<u64>,
    pub checked_at: Option<u64>,
    pub issue: Option<QuotaIssue>,
    pub windows: Vec<QuotaWindow>,
    pub revision: u64,
    pub history_unavailable: bool,
    #[serde(skip)]
    checked_sequence: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaView {
    pub observed_at: Option<u64>,
    pub checked_at: Option<u64>,
    pub stale: bool,
    pub issue: Option<QuotaIssue>,
    pub windows: Vec<QuotaWindowView>,
    pub revision: u64,
    pub history_unavailable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaWindowView {
    pub id: String,
    pub span_secs: u64,
    pub used_percent: f64,
    pub resets_at: Option<u64>,
    pub observed_at: u64,
    pub source: ObservationSource,
    pub stale: bool,
}

impl QuotaState {
    pub fn apply_read(
        &mut self,
        subject: &str,
        checked_at: u64,
        sequence: u64,
        windows: Vec<QuotaWindow>,
        issue: Option<QuotaIssue>,
    ) -> Vec<QuotaWindow> {
        self.select_subject(subject);
        if sequence >= self.checked_sequence {
            self.checked_at = Some(checked_at);
            self.checked_sequence = sequence;
            self.issue = issue;
        }
        self.merge(windows)
    }

    pub fn observe(&mut self, subject: &str, windows: Vec<QuotaWindow>) -> Vec<QuotaWindow> {
        self.select_subject(subject);
        self.merge(windows)
    }

    fn select_subject(&mut self, subject: &str) {
        if self.subject != subject {
            *self = Self {
                subject: subject.to_string(),
                revision: self.revision.saturating_add(1),
                ..Self::default()
            };
        }
    }

    fn merge(&mut self, windows: Vec<QuotaWindow>) -> Vec<QuotaWindow> {
        let mut accepted = Vec::new();
        for window in windows {
            if !window.used_percent.is_finite() || window.span_secs == 0 {
                continue;
            }
            if let Some(existing) = self.windows.iter_mut().find(|w| w.id == window.id) {
                if existing.sequence >= window.sequence {
                    continue;
                }
                *existing = window.clone();
            } else {
                self.windows.push(window.clone());
            }
            accepted.push(window);
        }
        self.windows.sort_by_key(|w| w.span_secs);
        self.observed_at = self.windows.iter().map(|w| w.observed_at).max();
        if !accepted.is_empty() {
            self.revision = self.revision.saturating_add(1);
        }
        accepted
    }

    pub fn view(&self, now: u64) -> QuotaView {
        QuotaView {
            observed_at: self.observed_at,
            checked_at: self.checked_at,
            stale: !self.windows.iter().any(|w| w.fresh(now)),
            issue: self.issue,
            revision: self.revision,
            history_unavailable: self.history_unavailable,
            windows: self
                .windows
                .iter()
                .map(|w| QuotaWindowView {
                    id: w.id.clone(),
                    span_secs: w.span_secs,
                    used_percent: w.used_percent,
                    resets_at: w.resets_at,
                    observed_at: w.observed_at,
                    source: w.source,
                    stale: !w.fresh(now),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(at: u64, seq: u64, used: f64) -> QuotaWindow {
        QuotaWindow::new(18_000, used, Some(2000), at, seq, ObservationSource::Api).unwrap()
    }

    #[test]
    fn cached_or_late_reads_cannot_replace_a_newer_observation() {
        let mut state = QuotaState::default();
        state.apply_read("first", 1000, 1, vec![window(1000, 1, 20.0)], None);
        assert!(state
            .apply_read("first", 1000, 1, vec![window(1000, 1, 20.0)], None)
            .is_empty());
        state.observe("first", vec![window(1001, 2, 30.0)]);
        state.apply_read("first", 1000, 1, vec![window(1000, 1, 20.0)], None);
        assert_eq!(state.observed_at, Some(1001));
        assert_eq!(state.windows[0].used_percent, 30.0);
        state.apply_read("first", 1002, 3, vec![], Some(QuotaIssue::Network));
        assert_eq!(state.windows[0].used_percent, 30.0);
        assert_eq!(state.issue, Some(QuotaIssue::Network));
        assert!(!state.view(1002).stale);
        assert!(state.view(1301).stale);
        assert!(serde_json::to_value(state.view(1002))
            .unwrap()
            .get("subject")
            .is_none());
    }

    #[test]
    fn subject_changes_reset_observations_and_resets_make_windows_unknown() {
        let mut state = QuotaState::default();
        state.apply_read("first", 1000, 1, vec![window(1000, 1, 20.0)], None);
        assert!(!state.windows[0].fresh(2000));
        state.apply_read("second", 1001, 2, vec![], Some(QuotaIssue::Authentication));
        assert!(state.windows.is_empty());
        assert_eq!(state.observed_at, None);
        assert_eq!(state.checked_at, Some(1001));
        assert!(QuotaWindow::new(18_000, f64::NAN, None, 1, 1, ObservationSource::Api).is_none());
    }
}
