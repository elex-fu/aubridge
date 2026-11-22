//! Core quota sampling and identity-bound local observations.

use std::sync::Arc;

use credentials::planquota::{PlanAuth, PlanWindows};
use credentials::quota::{subject_tag, ObservationStamp, QuotaRead};
use futures_util::{stream, StreamExt};
use manifest::quota::{ObservationSource, QuotaIssue, QuotaState, QuotaWindow};
use manifest::schema::Manifest;
use manifest::state::ProviderState;

use super::GatewayState;

pub(crate) const POLL_SECS: u64 = 60;

impl GatewayState {
    pub async fn sample_quotas(&self) {
        tokio::join!(self.sample_wham_usage(), self.sample_plan_quotas());
    }

    pub async fn sample_wham_usage(&self) {
        let Some(fetcher) = &self.wham else {
            return;
        };
        let ids: Vec<String> = self
            .manifests
            .read()
            .expect("manifests lock")
            .values()
            .filter(|m| self.chatgpt.owns(m))
            .map(|m| m.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let Some(snapshot) = self.chatgpt.quota_snapshot() else {
            return;
        };
        let subject = snapshot.quota_subject();
        let read = fetcher.read(self.http(), &snapshot.access_token).await;
        let mut windows = Vec::new();
        if let Some(limits) = read
            .value
            .as_ref()
            .and_then(|usage| usage.rate_limit.as_ref())
        {
            for (window, fallback) in [
                (&limits.primary_window, 604_800),
                (&limits.secondary_window, 18_000),
            ] {
                if let Some(window) = window {
                    if let Some(used) = window.used_percent {
                        if let Some(window) = QuotaWindow::new(
                            window.limit_window_seconds.unwrap_or(fallback),
                            used,
                            window.reset_at.and_then(|at| u64::try_from(at).ok()),
                            read.stamp.at,
                            read.stamp.sequence,
                            ObservationSource::Api,
                        ) {
                            windows.push(window);
                        }
                    }
                }
            }
        }
        self.chatgpt.update_current_login(&snapshot, || {
            for id in &ids {
                self.commit_quota_read(id, &subject, read.stamp, windows.clone(), read.issue);
            }
        });
    }

    pub async fn sample_plan_quotas(&self) {
        let jobs: Vec<(Manifest, String, PlanAuth)> = self
            .manifests
            .read()
            .expect("manifests lock")
            .values()
            .filter_map(|m| {
                credentials::planquota::plan_source(&m.provider.base)
                    .map(|(endpoint, auth)| (m.clone(), endpoint, auth))
            })
            .collect();
        stream::iter(jobs)
            .map(|(manifest, endpoint, auth)| async move {
                let Ok((crate::route_plan::Credential::Key(key), version)) =
                    self.dispatch_credential(&manifest)
                else {
                    return;
                };
                let subject = api_subject(&manifest, &key);
                let fetcher = {
                    let mut fetchers = self.plan_fetchers.write().expect("plan fetchers lock");
                    Arc::clone(
                        fetchers
                            .entry(format!("{endpoint}|{auth:?}"))
                            .or_insert_with(|| {
                                Arc::new(credentials::planquota::PlanFetcher::new(endpoint, auth))
                            }),
                    )
                };
                let read = fetcher.read(self.http(), &key).await;
                let windows = plan_windows(&read);
                self.update_current_connection(&manifest.id, Some(&version), || {
                    if self
                        .resolve_api_key_for(&manifest.id, Some(&manifest))
                        .0
                        .as_deref()
                        != Some(key.as_str())
                    {
                        return;
                    }
                    self.commit_quota_read(&manifest.id, &subject, read.stamp, windows, read.issue);
                });
            })
            .buffer_unordered(2)
            .collect::<Vec<_>>()
            .await;
    }

    fn commit_quota_read(
        &self,
        id: &str,
        subject: &str,
        stamp: ObservationStamp,
        windows: Vec<QuotaWindow>,
        issue: Option<QuotaIssue>,
    ) {
        self.update_quota(id, |quota| {
            quota.apply_read(subject, stamp.at, stamp.sequence, windows, issue);
        });
    }

    pub(crate) fn commit_passive_quota(&self, id: &str, subject: &str, windows: Vec<QuotaWindow>) {
        if windows.is_empty() {
            return;
        }
        self.update_quota(id, |quota| {
            quota.observe(subject, windows);
        });
    }

    fn update_quota(&self, id: &str, update: impl FnOnce(&mut QuotaState)) {
        let (subject, windows) = {
            let mut providers = self.providers.write().expect("providers lock");
            let Some(source) = providers.get_mut(id) else {
                return;
            };
            update(&mut source.quota);
            source.util_5h = source
                .quota
                .windows
                .iter()
                .filter(|w| w.span_secs <= 86_400)
                .min_by_key(|w| w.span_secs)
                .map(|w| w.used_percent);
            source.util_7d = source
                .quota
                .windows
                .iter()
                .find(|w| w.span_secs == 604_800)
                .map(|w| w.used_percent);
            if !source.quota.windows.is_empty() {
                source.resets_at = source
                    .quota
                    .windows
                    .iter()
                    .filter_map(|w| w.resets_at)
                    .min();
            }
            (source.quota.subject.clone(), source.quota.windows.clone())
        };
        // Retrying the same original observations is idempotent and recovers
        // transient disk failures without manufacturing a new sampling time.
        let unavailable = self.ledger.as_ref().is_some_and(|ledger| {
            ledger.lock().map_or(true, |ledger| {
                windows
                    .iter()
                    .any(|window| ledger.note_quota_window(id, &subject, window).is_err())
            })
        });
        if let Some(source) = self.providers.write().expect("providers lock").get_mut(id) {
            source.quota.history_unavailable = unavailable;
        }
        self.mark_dirty(id);
    }

    pub(super) fn bind_quota_identity(&self, source: &mut ProviderState, manifest: &Manifest) {
        if !source.quota.subject.is_empty()
            && self.current_quota_subject(manifest).as_deref()
                != Some(source.quota.subject.as_str())
        {
            let revision = source.quota.revision.saturating_add(1);
            source.quota = QuotaState::default();
            source.quota.revision = revision;
            source.util_5h = None;
            source.util_7d = None;
        }
    }

    pub(crate) fn current_quota_subject(&self, manifest: &Manifest) -> Option<String> {
        if self.chatgpt.owns(manifest) {
            self.chatgpt
                .quota_snapshot()
                .map(|login| login.quota_subject())
        } else {
            self.resolve_api_key_for(&manifest.id, Some(manifest))
                .0
                .map(|key| api_subject(manifest, &key))
                .or_else(|| {
                    reqwest::Url::parse(&manifest.provider.base)
                        .ok()
                        .filter(|url| {
                            url.host_str().is_some_and(|host| {
                                matches!(host, "localhost" | "127.0.0.1" | "[::1]")
                            })
                        })
                        .map(|_| local_subject(manifest))
                })
        }
    }

    pub fn quota_history(&self, provider: Option<&str>, since: u64) -> Vec<ledger::QuotaPointRow> {
        self.quota_history_result(provider, since)
            .unwrap_or_default()
    }

    pub(crate) fn quota_history_result(
        &self,
        provider: Option<&str>,
        since: u64,
    ) -> Result<Vec<ledger::QuotaPointRow>, &'static str> {
        let subjects: Vec<_> = self
            .routing_sources()
            .into_iter()
            .filter(|(source, _)| provider.is_none_or(|id| source.id == id))
            .filter(|(source, _)| !source.quota.subject.is_empty())
            .map(|(source, _)| (source.id, source.quota.subject))
            .collect();
        let Some(ledger) = &self.ledger else {
            return Ok(Vec::new());
        };
        let ledger = ledger.lock().map_err(|_| "quota history unavailable")?;
        let mut points = ledger
            .quota_history(provider, since)
            .map_err(|_| "quota history unavailable")?;
        for (id, subject) in subjects {
            points.extend(
                ledger
                    .quota_subject_history(&id, &subject, since)
                    .map_err(|_| "quota history unavailable")?,
            );
        }
        Ok(points)
    }
}

pub(crate) fn api_subject(manifest: &Manifest, key: &str) -> String {
    subject_tag(
        &format!("provider:{}:{}", manifest.id, manifest.provider.base),
        key,
    )
}

pub(crate) fn local_subject(manifest: &Manifest) -> String {
    subject_tag(
        "local-provider",
        &format!("{}:{}", manifest.id, manifest.provider.base),
    )
}

fn plan_windows(read: &QuotaRead<PlanWindows>) -> Vec<QuotaWindow> {
    let Some(plan) = &read.value else {
        return Vec::new();
    };
    [
        (plan.span_5h.unwrap_or(18_000), plan.w5h, plan.reset_5h),
        (604_800, plan.w7d, plan.reset_7d),
    ]
    .into_iter()
    .filter_map(|(span, used, reset)| {
        QuotaWindow::new(
            span,
            used?,
            reset,
            read.stamp.at,
            read.stamp.sequence,
            ObservationSource::Api,
        )
    })
    .collect()
}
