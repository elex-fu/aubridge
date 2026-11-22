//! Decision 51: bounded private CLI jobs; GET only reads cached metadata.
mod discovery;
mod parser;
mod process;
mod project;
use crate::state::GatewayState;
pub(crate) use discovery::{cached_discovery, discover};
pub(crate) use process::capture_for_probe;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub(crate) struct ValidationService {
    jobs: Mutex<HashMap<String, Entry>>,
    discovery: Mutex<HashMap<String, (String, agents::readiness::Discovery)>>,
}
struct Entry {
    job: Job,
    identity: String,
    cancel: tokio::sync::watch::Sender<bool>,
    sequence: u64,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Job {
    pub job_id: String,
    pub agent_id: String,
    pub scope: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_alias: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ValidationRequest {
    pub request_id: String,
    pub provider: String,
    pub served_model: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Evidence {
    pub cli_version: String,
    pub gateway_revision: String,
    pub session_id: String,
    pub requests: Vec<ValidationRequest>,
    pub tool_result_matched: bool,
    pub final_exact: bool,
}
pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn active(status: &str) -> bool {
    matches!(status, "queued" | "running")
}
pub(crate) fn get(state: &GatewayState, id: &str) -> Option<Job> {
    state
        .agent_validation
        .jobs
        .lock()
        .ok()?
        .get(id)
        .map(|e| e.job.clone())
}
pub(crate) fn cancel(state: &GatewayState, id: &str) -> Option<Job> {
    let mut jobs = state.agent_validation.jobs.lock().ok()?;
    let entry = jobs.get_mut(id)?;
    if active(&entry.job.status) {
        let _ = entry.cancel.send(true);
    }
    Some(entry.job.clone())
}
pub(crate) fn summary(state: &GatewayState, agent: &str) -> serde_json::Value {
    let Ok(jobs) = state.agent_validation.jobs.lock() else {
        return serde_json::json!({"status":"not_run","valid":false});
    };
    let Some(entry) = jobs
        .values()
        .filter(|e| e.job.agent_id == agent)
        .max_by_key(|e| e.sequence)
    else {
        return serde_json::json!({"status":"not_run","valid":false});
    };
    let valid = entry.job.status == "passed"
        && discovery::identity(state, agent).is_ok_and(|i| i == entry.identity)
        && entry
            .job
            .project_handle
            .as_deref()
            .is_none_or(|handle| crate::agent_project::valid(state, agent, handle));
    let mut value = serde_json::json!({"last_job_id":entry.job.job_id,"status":entry.job.status,"scope":entry.job.scope,"observed_at":entry.job.finished_at,"valid":valid});
    if let Some(handle) = &entry.job.project_handle {
        value["project_handle"] = serde_json::json!(handle);
        value["project_alias"] = serde_json::json!(entry.job.project_alias);
    }
    value
}
pub(crate) fn start(state: Arc<GatewayState>, agent: String) -> Result<Job, String> {
    start_inner(state, agent, None, false)
}
pub(crate) fn start_project(
    state: Arc<GatewayState>,
    agent: String,
    handle: String,
    allow_project_temp_file: bool,
) -> Result<Job, String> {
    start_inner(state, agent, Some(handle), allow_project_temp_file)
}
fn start_inner(
    state: Arc<GatewayState>,
    agent: String,
    project_handle: Option<String>,
    allow_project_temp_file: bool,
) -> Result<Job, String> {
    if agents::readiness::program_name(&agent).is_none() {
        return Err("unknown_agent".into());
    }
    let activity = state.begin_update_activity().map_err(str::to_owned)?;
    let project_alias = match project_handle.as_deref() {
        Some(handle) => Some(crate::agent_project::resolve(&state, &agent, handle)?.1),
        None => None,
    };
    let identity = discovery::identity(&state, &agent)?;
    let mut jobs = state
        .agent_validation
        .jobs
        .lock()
        .map_err(|_| "validation_unavailable")?;
    if jobs.values().any(|e| active(&e.job.status)) {
        return Err("validation_busy".into());
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| "random_unavailable")?;
    let id = random
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    let job = Job {
        job_id: id.clone(),
        agent_id: agent,
        scope: if project_handle.is_some() {
            "project"
        } else {
            "private_probe"
        }
        .into(),
        status: "queued".into(),
        reason: None,
        started_at: Some(now()),
        finished_at: None,
        evidence: None,
        project_handle,
        project_alias,
    };
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    // Retain bounded metadata only; no process output, file body or nonce.
    if jobs.len() >= 32 {
        if let Some(oldest) = jobs
            .values()
            .filter(|e| !active(&e.job.status))
            .min_by_key(|e| e.sequence)
            .map(|e| e.job.job_id.clone())
        {
            jobs.remove(&oldest);
        }
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    jobs.insert(
        id.clone(),
        Entry {
            job: job.clone(),
            identity,
            cancel,
            sequence,
        },
    );
    drop(jobs);
    let worker_job = job.clone();
    tokio::spawn(async move {
        let _activity = activity;
        if let Ok(mut jobs) = state.agent_validation.jobs.lock() {
            if let Some(e) = jobs.get_mut(&id) {
                e.job.status = "running".into();
            }
        }
        let result = if let Some(handle) = worker_job.project_handle.as_deref() {
            project::run(
                &state,
                &worker_job.agent_id,
                &id,
                handle,
                allow_project_temp_file,
                receiver,
            )
            .await
        } else {
            process::run(&state, &id, receiver).await
        };
        if let Ok(mut jobs) = state.agent_validation.jobs.lock() {
            if let Some(e) = jobs.get_mut(&id) {
                match result {
                    Ok(evidence) => {
                        e.job.status = "passed".into();
                        e.job.evidence = Some(evidence);
                    }
                    Err(reason) => {
                        e.job.status = match reason.as_str() {
                            "cancelled" => "cancelled",
                            "timed_out" => "timed_out",
                            _ => "failed",
                        }
                        .into();
                        e.job.reason = Some(reason);
                    }
                }
                e.job.finished_at = Some(now());
            }
        }
    });
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latest_job_survives_wall_clock_rollback() {
        let state = GatewayState::from_manifests(std::collections::BTreeMap::new(), "test");
        let make = |id: &str, at| Job {
            job_id: id.into(),
            agent_id: "codex".into(),
            scope: "private_probe".into(),
            status: "failed".into(),
            reason: None,
            started_at: Some(at),
            finished_at: Some(at),
            evidence: None,
            project_handle: None,
            project_alias: None,
        };
        let (sender, _receiver) = tokio::sync::watch::channel(false);
        let mut jobs = state.agent_validation.jobs.lock().unwrap();
        jobs.insert(
            "old".into(),
            Entry {
                job: make("old", 100),
                identity: String::new(),
                cancel: sender.clone(),
                sequence: 1,
            },
        );
        jobs.insert(
            "new".into(),
            Entry {
                job: make("new", 99),
                identity: String::new(),
                cancel: sender,
                sequence: 2,
            },
        );
        drop(jobs);
        assert_eq!(summary(&state, "codex")["last_job_id"], "new");
    }
}
