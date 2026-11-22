//! Decision 55 orchestration. Pending archives never enter the provider registry.
use super::{custom_manifest, provider_state_of, GatewayState};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use credentials::usage_policy::CostIntent;
use credentials::work_config_backup::{
    self as backup, Archive, Group, Payload, PendingRestore, RestorePlan, Source, SourceKind, Undo,
    WiringIntent,
};
use manifest::schema::{AuthKind, Manifest, WireProtocol};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(crate) struct BackupService {
    inner: Mutex<Store>,
}
#[derive(Default)]
struct Store {
    loaded: bool,
    next: u64,
    previews: BTreeMap<String, Preview>,
    restores: BTreeMap<String, Receipt>,
}
struct Preview {
    path: PathBuf,
    file_digest: String,
    plan: RestorePlan,
}
struct RuntimeCandidate {
    manifests: BTreeMap<String, Manifest>,
    settings: credentials::settings::Settings,
    bound_keys: BTreeMap<String, String>,
}
struct Receipt {
    undo: Undo,
    confirmed: BTreeSet<String>,
    revision: String,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    path: Option<PathBuf>,
    preview_id: Option<String>,
    restore_id: Option<String>,
    source_id: Option<String>,
    expected_identity: Option<String>,
    target_revision: Option<String>,
    #[serde(default)]
    confirm_endpoint: bool,
    #[serde(default)]
    confirm_key_write: bool,
    key: Option<String>,
}
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn rev(value: &Value) -> Result<String, String> {
    Ok(backup::digest(&serde_json::to_vec(value).map_err(err)?))
}
fn source(m: &Manifest, kind: SourceKind) -> Source {
    Source {
        id: m.id.clone(),
        source: kind,
        base: m.provider.base.clone(),
        auth: match m.provider.auth {
            AuthKind::Bearer => "bearer",
            AuthKind::ApiKey => "api-key",
            AuthKind::ClaudeCli => "claude_cli",
        }
        .into(),
        protocol: match m.provider.protocol {
            WireProtocol::Chat => "chat",
            WireProtocol::Anthropic => "anthropic",
            WireProtocol::Responses => "responses",
        }
        .into(),
        kind: match m.kind {
            manifest::ProviderKind::FreeTier => "free_tier",
            manifest::ProviderKind::Subscription => "subscription",
        }
        .into(),
        models: m.free_models.clone(),
    }
}
fn custom(s: &Source) -> credentials::settings::CustomProvider {
    credentials::settings::CustomProvider {
        id: s.id.clone(),
        base: s.base.clone(),
        auth: s.auth.clone(),
        protocol: s.protocol.clone(),
        kind: s.kind.clone(),
        models: s.models.clone(),
    }
}
fn presets() -> Vec<Source> {
    registry::presets()
        .iter()
        .map(|m| source(m, SourceKind::Preset))
        .collect()
}
fn pending(document: &Value) -> Result<PendingRestore, String> {
    serde_json::from_value(document["pending_work_config_restore"].clone())
        .map_err(|_| "没有待完成恢复".into())
}
fn io<T>(value: std::io::Result<T>) -> Result<T, String> {
    value.map_err(err)
}
fn need<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str, String> {
    value.as_deref().ok_or_else(|| format!("缺少 {name}"))
}

pub(crate) async fn route(
    state: Arc<GatewayState>,
    headers: HeaderMap,
    action: String,
    body: String,
) -> Response<Body> {
    if !state.is_valid_admin_key(crate::security::caller_key(&headers).as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"admin key required"})),
        )
            .into_response();
    }
    let request = if body.is_empty() {
        Ok(Request::default())
    } else {
        serde_json::from_str::<Request>(&body)
    };
    let result = request
        .map_err(err)
        .and_then(|request| state.work_config_action(&action, request));
    match result {
        Ok(view) => Json(view).into_response(),
        Err(message) => (StatusCode::CONFLICT, Json(json!({"error":message}))).into_response(),
    }
}
impl GatewayState {
    fn work_config_action(&self, action: &str, request: Request) -> Result<Value, String> {
        let (config, _) = self.settings.as_ref().ok_or("设置存储未接线")?;
        let _agents = self.agents_io.lock().map_err(|_| "客户端配置锁不可用")?;
        let _settings = self.settings_io.lock().map_err(|_| "设置锁不可用")?;
        let mut store = self.backup.inner.lock().map_err(|_| "恢复服务不可用")?;
        let document = io(backup::read_document(config))?;
        if !store.loaded {
            self.recover_restore_receipt(config, &document, &mut store)?;
            store.loaded = true;
        }
        let result = match action {
            "status" => {
                let mut view = self.backup_view(&document)?;
                if let Some((id, _)) = store.restores.iter().next_back() {
                    view["restore_id"] = json!(id);
                }
                Ok(view)
            }
            "export" => {
                let path = request.path.ok_or("请选择备份路径")?;
                let payload = self.backup_payload(&document)?;
                let encoded = io(Archive::encode(payload))?;
                io(backup::export(
                    &path,
                    config.parent().ok_or("应用目录不可用")?,
                    &encoded,
                ))?;
                Ok(
                    json!({"status":"exported","path":path,"scope":"same_machine_work_config","credentials_exported":false}),
                )
            }
            "preview" => {
                let path = request
                    .path
                    .ok_or("请选择归档")?
                    .canonicalize()
                    .map_err(err)?;
                let content = io(backup::read_archive_file(&path))?;
                let archive = io(Archive::decode(&content))?;
                let plan = io(RestorePlan::preview(&archive, &document, &presets()))?;
                for source in &plan.pending.payload.sources {
                    self.validate_restore_source_identity(source)?;
                    if source.source == SourceKind::Custom {
                        custom_manifest(&custom(source))?;
                    }
                }
                store.next += 1;
                let id = format!("preview-{}-{}", std::process::id(), store.next);
                let mut view = json!({"preview_id":id,"digest":plan.pending.archive_digest,"target_revision":plan.target_revision,"payload":plan.pending.payload,"status":"awaiting_confirmation","client_wiring_status":"intent_only"});
                merge_view(
                    &mut view,
                    self.cost_restore_view(
                        &document,
                        plan.pending.payload.cost_intent,
                        Some(plan.pending.payload.cost_intent.is_some()),
                        true,
                    )?,
                );
                if store.previews.len() >= 8 {
                    if let Some(oldest) = store.previews.keys().next().cloned() {
                        store.previews.remove(&oldest);
                    }
                }
                store.previews.insert(
                    id,
                    Preview {
                        path,
                        file_digest: backup::digest(&content),
                        plan,
                    },
                );
                Ok(view)
            }
            "stage" => {
                let id = need(&request.preview_id, "preview_id")?;
                let preview = store.previews.get(id).ok_or("预览已失效，请重新预览")?;
                let current_bytes = io(backup::read_archive_file(&preview.path))?;
                if backup::digest(&current_bytes) != preview.file_digest {
                    return Err("归档已改变，请重新预览".into());
                }
                let undo = io(preview.plan.commit(config))?;
                store.previews.remove(id);
                store.next += 1;
                let id = format!("restore-{}-{}", std::process::id(), store.next);
                let current = io(backup::read_document(config))?;
                store.restores.clear();
                store.restores.insert(
                    id.clone(),
                    Receipt {
                        undo,
                        confirmed: BTreeSet::new(),
                        revision: rev(&current)?,
                    },
                );
                let mut view = self.backup_view(&current)?;
                view["restore_id"] = json!(id);
                Ok(view)
            }
            "rebind" | "supply-key" => {
                if !request.confirm_endpoint {
                    return Err("请确认完整端点、协议、鉴权方式及本机凭据来源".into());
                }
                let id = need(&request.restore_id, "restore_id")?;
                let receipt = store
                    .restores
                    .get_mut(id)
                    .ok_or("恢复确认已失效，请取消后重新预览")?;
                if receipt.revision != rev(&document)?
                    || request.target_revision.as_deref() != Some(receipt.revision.as_str())
                {
                    return Err("目标配置已改变，请重新查看恢复状态".into());
                }
                let mut pending = pending(&document)?;
                let source_id = need(&request.source_id, "source_id")?;
                let source = pending
                    .payload
                    .sources
                    .iter()
                    .find(|s| s.id == source_id)
                    .ok_or("归档来源不存在")?;
                if request.expected_identity.as_deref() != Some(io(source.identity())?.as_str()) {
                    return Err("来源身份已改变".into());
                }
                if action == "supply-key" {
                    if !request.confirm_key_write {
                        return Err("请明确确认写入或替换此来源 ID 的本机钥匙串凭据".into());
                    }
                    let key = request
                        .key
                        .as_deref()
                        .filter(|key| !key.trim().is_empty())
                        .ok_or("请输入来源 Key")?;
                    self.validate_restore_source_identity(source)?;
                    if source.id == "chatgpt" || source.auth == "claude_cli" {
                        return Err("此来源须通过官方本机登录补齐".into());
                    }
                    self.key_store
                        .as_ref()
                        .ok_or("本机钥匙串未接线")?
                        .set(&source.id, key)
                        .map_err(|_| "未能写入本机钥匙串，请检查授权后重试")?;
                }
                self.verify_restore_source(source, &pending.payload.chatgpt_login_source)?;
                let mut confirmed = receipt.confirmed.clone();
                confirmed.insert(source.id.clone());
                pending.bound_sources = confirmed.iter().cloned().collect();
                io(backup::patch_if_revision(
                    config,
                    &BTreeMap::from([(
                        "pending_work_config_restore".into(),
                        Some(serde_json::to_value(&pending).map_err(err)?),
                    )]),
                    &receipt.revision,
                ))?;
                receipt.confirmed = confirmed;
                receipt.undo.after.insert(
                    "pending_work_config_restore".into(),
                    Some(serde_json::to_value(pending).map_err(err)?),
                );
                let current = io(backup::read_document(config))?;
                receipt.revision = rev(&current)?;
                self.backup_view(&current)
            }
            "promote" => {
                let id = need(&request.restore_id, "restore_id")?;
                let receipt = store
                    .restores
                    .get_mut(id)
                    .ok_or("恢复确认已失效，请取消后重新预览")?;
                if receipt.revision != rev(&document)?
                    || request.target_revision.as_deref() != Some(receipt.revision.as_str())
                {
                    return Err("目标配置已改变，请重新预览".into());
                }
                let pending = pending(&document)?;
                let mut bound_keys = BTreeMap::new();
                for source in pending
                    .payload
                    .sources
                    .iter()
                    .filter(|s| !pending.payload.paused_providers.contains(&s.id))
                {
                    if !receipt.confirmed.contains(&source.id) {
                        return Err(format!("{} 尚未明确绑定凭据", source.id));
                    }
                    if let Some(key) =
                        self.verify_restore_source(source, &pending.payload.chatgpt_login_source)?
                    {
                        bound_keys.insert(source.id.clone(), key);
                    }
                }
                let changes = self.promotion_changes(&document, &pending)?;
                let next = merged(&document, &changes)?;
                let mut runtime = self.restore_runtime_candidate(&next)?;
                let cost_view = self.cost_restore_view(
                    &next,
                    pending.payload.cost_intent,
                    Some(pending.payload.cost_intent.is_some()),
                    false,
                )?;
                runtime.bound_keys = bound_keys;
                let undo = io(backup::patch_if_revision(
                    config,
                    &changes,
                    &receipt.revision,
                ))?;
                for (key, value) in undo.before {
                    receipt.undo.before.entry(key).or_insert(value);
                }
                receipt.undo.after.extend(undo.after);
                self.apply_restore_runtime(runtime, &next);
                receipt.revision = rev(&next)?;
                let mut view = json!({"status":if next.get("pending_work_config_restore").is_some_and(|v|!v.is_null()) {"restore_pending"} else {"configuration_restored"},"restore_id":id,"target_revision":receipt.revision,"client_wiring":pending.payload.client_wiring,"client_wiring_status":"intent_only","workspace_status":"real_task_required"});
                merge_view(&mut view, cost_view);
                Ok(view)
            }
            "undo" | "cancel" => {
                let id = need(&request.restore_id, "restore_id")?;
                let receipt = store.restores.get(id).ok_or("撤销回执不可用")?;
                // Validate runtime before the single disk commit.
                if receipt
                    .undo
                    .after
                    .iter()
                    .any(|(key, value)| document.get(key) != value.as_ref())
                {
                    return Err("恢复后配置已外改，拒绝覆盖".into());
                }
                let next = merged(&document, &receipt.undo.before)?;
                let runtime = self.restore_runtime_candidate(&next)?;
                io(receipt.undo.apply(config))?;
                self.apply_restore_runtime(runtime, &next);
                store.restores.remove(id);
                Ok(json!({"status":"undone","target_revision":rev(&next)?}))
            }
            _ => Err("不支持的恢复操作".into()),
        };
        result.map(|mut view| {
            if let Some((id, receipt)) = store.restores.iter().next_back() {
                view["restore_id"] = json!(id);
                if let Some(pending) = view.get_mut("pending").filter(|v| v.is_object()) {
                    pending["bound_sources"] = json!(receipt.confirmed);
                }
                if let Some(sources) = view.get_mut("sources").and_then(Value::as_array_mut) {
                    for source in sources {
                        source["credential_status"] = json!(if source["id"]
                            .as_str()
                            .is_some_and(|id| receipt.confirmed.contains(id))
                        {
                            "bound"
                        } else {
                            "unconfirmed"
                        });
                    }
                }
            }
            view
        })
    }
    fn recover_restore_receipt(
        &self,
        config: &Path,
        document: &Value,
        store: &mut Store,
    ) -> Result<(), String> {
        let journal = config.with_file_name("work-config-restore-before.json");
        let history: Vec<Undo> = match backup::load_private(&journal) {
            Ok(history) => history,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err("本机恢复回执不可读，请保留现场后重新预览".into()),
        };
        let mut aggregate: Option<Undo> = None;
        let mut matching = None;
        for transaction in history {
            if transaction
                .after
                .contains_key("pending_work_config_restore")
                && transaction
                    .before
                    .get("pending_work_config_restore")
                    .is_none_or(|v| v.as_ref().is_none_or(Value::is_null))
            {
                aggregate = Some(Undo {
                    before: BTreeMap::new(),
                    after: BTreeMap::new(),
                });
            }
            if let Some(aggregate) = aggregate.as_mut() {
                for (key, value) in transaction.before {
                    aggregate.before.entry(key).or_insert(value);
                }
                aggregate.after.extend(transaction.after);
                if aggregate
                    .after
                    .iter()
                    .all(|(key, value)| document.get(key) == value.as_ref())
                {
                    matching = (aggregate.before != aggregate.after).then(|| aggregate.clone());
                }
            }
        }
        if let Some(undo) = matching {
            let revision = rev(document)?;
            let id = format!("restore-recovered-{}", &revision[..16]);
            store.restores.insert(
                id,
                Receipt {
                    undo,
                    confirmed: BTreeSet::new(),
                    revision,
                },
            );
        }
        Ok(())
    }
    fn backup_view(&self, document: &Value) -> Result<Value, String> {
        let pending = document
            .get("pending_work_config_restore")
            .filter(|v| !v.is_null())
            .cloned();
        let sources = pending.as_ref().map(|value| serde_json::from_value::<PendingRestore>(value.clone()).map_err(err)).transpose()?.map(|p| p.payload.sources.into_iter().map(|s| Ok(json!({"id":s.id,"base":s.base,"auth":s.auth,"protocol":s.protocol,"kind":s.kind,"expected_identity":io(s.identity())?,"credential_source":if s.id == "chatgpt" {"所选 CLI 的本机登录（只读）"}else{"Keychain · aubridge/provider-id"},"credential_status":"unconfirmed","can_supply_key":s.id != "chatgpt" && s.auth != "claude_cli"}))).collect::<Result<Vec<_>,String>>()).transpose()?.unwrap_or_default();
        let intents: Vec<WiringIntent> = document
            .get("restored_client_wiring_intents")
            .map(|value| serde_json::from_value(value.clone()).map_err(err))
            .transpose()?
            .unwrap_or_default();
        let archive_cost = pending
            .as_ref()
            .map(|value| serde_json::from_value::<PendingRestore>(value.clone()).map_err(err))
            .transpose()?
            .map(|restore| restore.payload.cost_intent);
        let mut view = json!({"status":if pending.is_some(){"restore_pending"}else{"idle"},"pending":pending,"sources":sources,"dormant":document.get("dormant_work_config_restores"),"target_revision":rev(document)?,"workspace_status":"not_verified","restored_client_wiring_intents":intents,"client_wiring_status":"intent_only"});
        merge_view(
            &mut view,
            self.cost_restore_view(
                document,
                archive_cost.flatten(),
                archive_cost.map(|cost| cost.is_some()),
                false,
            )?,
        );
        Ok(view)
    }
    fn cost_restore_view(
        &self,
        document: &Value,
        archived: Option<CostIntent>,
        recorded: Option<bool>,
        preview: bool,
    ) -> Result<Value, String> {
        let settings: credentials::settings::Settings =
            serde_json::from_value(document.clone()).map_err(err)?;
        let marker: Option<CostIntent> = document
            .get("restored_desired_cost_intent")
            .map(|value| serde_json::from_value(value.clone()).map_err(err))
            .transpose()?;
        let requested = archived.or(marker);
        let effective = if preview && archived.is_some() {
            Some(CostIntent::ConfirmedFreeOnly)
        } else {
            settings.cost_intent
        };
        let needs = requested == Some(CostIntent::FreeThenSubscription)
            && effective != Some(CostIntent::FreeThenSubscription);
        Ok(
            json!({"requested_cost_intent":requested,"effective_cost_intent":effective,"needs_cost_confirmation":needs,"cost_intent_recorded":recorded}),
        )
    }
    fn backup_payload(&self, document: &Value) -> Result<Payload, String> {
        let settings: credentials::settings::Settings =
            serde_json::from_value(document.clone()).map_err(err)?;
        let customs = settings
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect::<BTreeSet<_>>();
        let manifests = self.manifests.read().map_err(|_| "来源不可用")?;
        let mut sources = Vec::new();
        for manifest in manifests.values() {
            if customs.contains(&manifest.id) {
                sources.push(source(manifest, SourceKind::Custom));
            } else if registry::presets().iter().any(|p| p.id == manifest.id)
                && (settings.model_selections.contains_key(&manifest.id)
                    || settings.paused_providers.contains(&manifest.id)
                    || self
                        .keys
                        .read()
                        .map_err(|_| "凭据状态不可用")?
                        .contains_key(&manifest.id)
                    || self.chatgpt.owns(manifest) && self.chatgpt.has_login())
            {
                sources.push(source(manifest, SourceKind::Preset));
            }
        }
        let models = manifests
            .values()
            .flat_map(|m| m.free_models.clone())
            .map(|m| (m.clone(), m))
            .collect();
        let client_wiring: Vec<WiringIntent> = self
            .agents
            .as_ref()
            .map(|paths| {
                agents::wirings(paths, self.port(), models)
                    .iter()
                    .map(|w| WiringIntent {
                        agent: w.id().into(),
                        enabled: w.wired(),
                        model: if w.wired() {
                            w.current_model().unwrap_or_default()
                        } else {
                            String::new()
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
        let required_models = settings
            .model_groups
            .iter()
            .flat_map(|group| group.members.iter().cloned())
            .chain(
                client_wiring
                    .iter()
                    .map(|intent: &WiringIntent| intent.model.clone()),
            )
            .collect::<BTreeSet<_>>();
        for manifest in manifests.values() {
            if !sources.iter().any(|s| s.id == manifest.id)
                && registry::presets().iter().any(|p| p.id == manifest.id)
                && manifest
                    .free_models
                    .iter()
                    .any(|m| required_models.contains(m))
            {
                sources.push(source(manifest, SourceKind::Preset));
            }
        }
        let mut payload = Payload {
            sources,
            groups: settings
                .model_groups
                .into_iter()
                .map(|g| Group {
                    name: g.name,
                    members: g.members,
                    rule: g.rule,
                })
                .collect(),
            model_selections: settings.model_selections,
            paused_providers: settings.paused_providers.into_iter().collect(),
            chatgpt_login_source: settings.chatgpt_login_source,
            client_wiring,
            cost_intent: settings.cost_intent,
        };
        if let Some(value) = document.get("restored_client_wiring_intents") {
            let intents: Vec<WiringIntent> = serde_json::from_value(value.clone()).map_err(err)?;
            for intent in intents {
                if let Some(current) = payload
                    .client_wiring
                    .iter_mut()
                    .find(|w| w.agent == intent.agent && !w.enabled)
                {
                    *current = intent;
                } else if !payload
                    .client_wiring
                    .iter()
                    .any(|w| w.agent == intent.agent)
                {
                    payload.client_wiring.push(intent);
                }
            }
        }
        if let Some(value) = document.get("dormant_work_config_restores") {
            let records: Vec<PendingRestore> =
                serde_json::from_value(value.clone()).map_err(err)?;
            for record in records {
                let mut dormant_models = BTreeSet::new();
                for source in record
                    .payload
                    .sources
                    .iter()
                    .filter(|s| record.payload.paused_providers.contains(&s.id))
                {
                    if let Some(current) = payload.sources.iter().find(|s| s.id == source.id) {
                        if !io(current.same_connection(source))? {
                            return Err("现有来源与休眠意图冲突，请先处理来源配置".into());
                        }
                    } else {
                        dormant_models.extend(source.models.clone());
                        payload.sources.push(source.clone());
                        payload.paused_providers.push(source.id.clone());
                        if let Some(models) = record.payload.model_selections.get(&source.id) {
                            payload
                                .model_selections
                                .insert(source.id.clone(), models.clone());
                        }
                    }
                }
                for group in record
                    .payload
                    .groups
                    .iter()
                    .filter(|g| g.members.iter().any(|m| dormant_models.contains(m)))
                {
                    if let Some(current) = payload.groups.iter().find(|g| g.name == group.name) {
                        if current != group {
                            return Err("现有分组与休眠意图冲突，请先处理分组配置".into());
                        }
                    } else {
                        payload.groups.push(group.clone());
                    }
                }
            }
        }
        payload.paused_providers.sort();
        payload.paused_providers.dedup();
        Ok(payload)
    }
    fn validate_restore_source_identity(&self, source: &Source) -> Result<(), String> {
        if source.source == SourceKind::Custom
            && self.plugins_dir.as_ref().is_some_and(|dir| {
                registry::plugins::scan_dir(dir)
                    .plugins
                    .iter()
                    .any(|m| m.id == source.id)
            })
        {
            return Err("同 ID 已安装插件拥有此凭据引用，即使禁用也不能重新绑定".into());
        }
        if self.manifest(&source.id).is_some() {
            let custom = self
                .custom_ids
                .read()
                .map_err(|_| "来源归属不可用")?
                .contains(&source.id);
            let plugin = self
                .plugin_ids
                .read()
                .map_err(|_| "插件归属不可用")?
                .contains(&source.id);
            if plugin || (source.source == SourceKind::Custom) != custom {
                return Err("同 ID 来源归属冲突，不能重新绑定其凭据".into());
            }
        }
        if source.source == SourceKind::Preset {
            let compiled = presets();
            let trusted = compiled
                .iter()
                .find(|p| p.id == source.id)
                .ok_or("内置来源不可用")?;
            if !io(source.same_connection(trusted))? {
                return Err("内置来源身份不匹配".into());
            }
        }
        if let Some(current) = self.manifest(&source.id) {
            if !io(source.same_connection(&self::source(&current, source.source.clone())))? {
                return Err("同 ID 本机来源身份已改变".into());
            }
        }
        Ok(())
    }
    fn verify_restore_source(
        &self,
        source: &Source,
        login_preference: &str,
    ) -> Result<Option<String>, String> {
        self.validate_restore_source_identity(source)?;
        if source.source == SourceKind::Preset {
            let compiled = presets();
            let trusted = compiled
                .iter()
                .find(|p| p.id == source.id)
                .ok_or("内置来源不可用")?;
            if !io(source.same_connection(trusted))? {
                return Err("内置来源身份不匹配".into());
            }
        }
        if let Some(current) = self.manifest(&source.id) {
            if !io(source.same_connection(&self::source(&current, source.source.clone())))? {
                return Err("同 ID 本机来源身份已改变".into());
            }
            if self.chatgpt.owns(&current) {
                let selected = if login_preference.is_empty() {
                    self.chatgpt.detect_login_source()
                } else {
                    crate::chatgpt::LoginSource::parse(login_preference)
                        .ok_or("登录来源偏好不支持")?
                };
                return self
                    .chatgpt
                    .has_login_for(selected)
                    .then_some(None)
                    .ok_or_else(|| "未取得所选来源的本机登录，请先补齐登录".into());
            }
            if current.provider.auth == AuthKind::ClaudeCli {
                return Err("Claude 登录须在普通来源设置单独启用，本恢复不授予权限".into());
            }
        }
        self.key_store
            .as_ref()
            .and_then(|store| store.get(&source.id))
            .filter(|key| !key.trim().is_empty())
            .map(Some)
            .ok_or_else(|| "未取得本机钥匙串凭据，请补齐后重试".into())
    }
    fn promotion_changes(
        &self,
        document: &Value,
        pending: &PendingRestore,
    ) -> Result<BTreeMap<String, Option<Value>>, String> {
        // Recheck identity after every possible intervening edit, never use archive claims as authority.
        let archive = Archive::decode(&Archive::encode(pending.payload.clone()).map_err(err)?)
            .map_err(err)?;
        let mut target = document.clone();
        target
            .as_object_mut()
            .ok_or("配置不是对象")?
            .remove("pending_work_config_restore");
        RestorePlan::preview(&archive, &target, &presets()).map_err(err)?;
        let mut settings: credentials::settings::Settings =
            serde_json::from_value(document.clone()).map_err(err)?;
        let mut dormant_sources = BTreeSet::new();
        for source in &pending.payload.sources {
            if source.source == SourceKind::Custom
                && pending.payload.paused_providers.contains(&source.id)
                && !settings.providers.iter().any(|p| p.id == source.id)
            {
                dormant_sources.insert(source.id.clone());
                continue;
            }
            if source.id != "chatgpt"
                && source.auth != "claude_cli"
                && !pending.payload.paused_providers.contains(&source.id)
            {
                settings.restored_keychain_only.insert(source.id.clone());
            }
            if source.source == SourceKind::Custom {
                let provider = custom(source);
                if let Some(current) = settings.providers.iter_mut().find(|p| p.id == source.id) {
                    *current = provider;
                } else {
                    settings.providers.push(provider);
                }
                settings.restored_keychain_only.insert(source.id.clone());
            }
        }
        let dormant_models = pending
            .payload
            .sources
            .iter()
            .filter(|s| dormant_sources.contains(&s.id))
            .flat_map(|s| s.models.iter().cloned())
            .collect::<BTreeSet<_>>();
        for group in &pending.payload.groups {
            if group.members.iter().any(|m| dormant_models.contains(m)) {
                continue;
            }
            let value = credentials::settings::ModelGroup {
                name: group.name.clone(),
                members: group.members.clone(),
                rule: group.rule.clone(),
            };
            if let Some(old) = settings
                .model_groups
                .iter_mut()
                .find(|g| g.name == group.name)
            {
                *old = value;
            } else {
                settings.model_groups.push(value);
            }
        }
        for (id, models) in &pending.payload.model_selections {
            if !dormant_sources.contains(id) {
                settings.model_selections.insert(id.clone(), models.clone());
            }
        }
        for source in &pending.payload.sources {
            if pending.payload.paused_providers.contains(&source.id) {
                settings.paused_providers.insert(source.id.clone());
            } else {
                settings.paused_providers.remove(&source.id);
            }
        }
        let mut dormant_records: Vec<PendingRestore> =
            match document.get("dormant_work_config_restores") {
                Some(value) => serde_json::from_value(value.clone()).map_err(err)?,
                None => Vec::new(),
            };
        if !dormant_sources.is_empty() {
            if dormant_records.len() >= 32 {
                return Err("休眠恢复意图数量已达上限，请先处理现有意图".into());
            }
            dormant_records.push(pending.clone());
        }
        let mut wiring: Vec<WiringIntent> = match document.get("restored_client_wiring_intents") {
            Some(value) => serde_json::from_value(value.clone()).map_err(err)?,
            None => Vec::new(),
        };
        for intent in &pending.payload.client_wiring {
            if let Some(current) = wiring.iter_mut().find(|w| w.agent == intent.agent) {
                *current = intent.clone();
            } else {
                wiring.push(intent.clone());
            }
        }
        let provider_ids = pending
            .payload
            .sources
            .iter()
            .filter(|s| s.source == SourceKind::Custom)
            .map(|s| s.id.clone())
            .collect();
        let group_names = pending
            .payload
            .groups
            .iter()
            .map(|g| g.name.clone())
            .collect();
        let providers = merge_named_items(
            document.get("providers"),
            &json!(settings.providers),
            "id",
            &provider_ids,
        )?;
        let groups = merge_named_items(
            document.get("model_groups"),
            &json!(settings.model_groups),
            "name",
            &group_names,
        )?;
        let mut changes = BTreeMap::from([
            ("providers".into(), Some(providers)),
            ("model_groups".into(), Some(groups)),
            (
                "model_selections".into(),
                Some(json!(settings.model_selections)),
            ),
            (
                "paused_providers".into(),
                Some(json!(settings.paused_providers)),
            ),
            (
                "chatgpt_login_source".into(),
                Some(json!(pending.payload.chatgpt_login_source)),
            ),
            (
                "restored_keychain_only".into(),
                Some(json!(settings.restored_keychain_only)),
            ),
            ("restored_client_wiring_intents".into(), Some(json!(wiring))),
            (
                "dormant_work_config_restores".into(),
                Some(json!(dormant_records)),
            ),
            ("pending_work_config_restore".into(), None),
        ]);
        match pending.payload.cost_intent {
            Some(CostIntent::ConfirmedFreeOnly) => {
                changes.insert(
                    "cost_intent".into(),
                    Some(json!(CostIntent::ConfirmedFreeOnly)),
                );
                changes.insert("restored_desired_cost_intent".into(), None);
            }
            Some(CostIntent::FreeThenSubscription) => {
                changes.insert(
                    "cost_intent".into(),
                    Some(json!(CostIntent::ConfirmedFreeOnly)),
                );
                changes.insert(
                    "restored_desired_cost_intent".into(),
                    Some(json!(CostIntent::FreeThenSubscription)),
                );
            }
            None => {}
        }
        Ok(changes)
    }
    fn restore_runtime_candidate(&self, document: &Value) -> Result<RuntimeCandidate, String> {
        let settings: credentials::settings::Settings =
            serde_json::from_value(document.clone()).map_err(err)?;
        let current = credentials::settings::Settings::load_checked(
            &self.settings.as_ref().ok_or("设置存储未接线")?.0,
        )
        .map_err(err)?;
        let mut manifests = self.manifests.read().map_err(|_| "来源不可用")?.clone();
        let old_custom = self.custom_ids.read().map_err(|_| "自定义来源不可用")?;
        for id in old_custom
            .iter()
            .filter(|id| !settings.providers.iter().any(|p| &p.id == *id))
        {
            manifests.remove(id);
        }
        for baseline in registry::presets() {
            if manifests.contains_key(&baseline.id)
                && current.model_selections.get(&baseline.id)
                    != settings.model_selections.get(&baseline.id)
            {
                manifests.insert(baseline.id.clone(), baseline);
            }
        }
        for provider in &settings.providers {
            let unchanged = current.providers.iter().any(|p| p == provider)
                && current.model_selections.get(&provider.id)
                    == settings.model_selections.get(&provider.id);
            if unchanged
                && old_custom.contains(&provider.id)
                && manifests.contains_key(&provider.id)
            {
                continue;
            }
            let m = custom_manifest(provider)?;
            if manifests.contains_key(&m.id) && !old_custom.contains(&m.id) {
                return Err("自定义源 ID 冲突".into());
            }
            manifests.insert(m.id.clone(), m);
        }
        for (id, models) in &settings.model_selections {
            if current.model_selections.get(id) == Some(models)
                && current.providers.iter().find(|p| &p.id == id)
                    == settings.providers.iter().find(|p| &p.id == id)
            {
                continue;
            }
            if let Some(m) = manifests
                .get_mut(id)
                .filter(|m| m.kind == manifest::ProviderKind::Subscription)
            {
                m.free_models = models.clone();
                m.model_facts.retain(|id, _| models.contains(id));
                m.context_windows.retain(|id, _| models.contains(id));
                m.models_without_tools.retain(|id| models.contains(id));
            }
        }
        let mut available = manifests
            .values()
            .flat_map(|m| m.free_models.iter().cloned())
            .collect::<BTreeSet<_>>();
        for group in &settings.model_groups {
            let rule = router::GroupRule::parse(&group.rule).ok_or("分组规则无效")?;
            if !group.members.iter().all(|member| {
                manifests
                    .values()
                    .any(|m| m.free_models.contains(member) && rule.allows_provider(m.kind))
            }) {
                return Err(format!("分组 {} 存在未满足的来源依赖", group.name));
            }
            available.insert(group.name.clone());
        }
        for (client, model) in self.managed_agent_models() {
            if model != crate::AUTO_MODEL && !available.contains(&model) {
                return Err(format!("恢复会移除 {client} 当前所需模型 {model}"));
            }
        }
        Ok(RuntimeCandidate {
            manifests,
            settings,
            bound_keys: BTreeMap::new(),
        })
    }
    fn apply_restore_runtime(&self, candidate: RuntimeCandidate, _document: &Value) {
        // All fallible parsing/validation and persistence finished before publishing runtime state.
        let RuntimeCandidate {
            manifests: candidate,
            settings,
            bound_keys,
        } = candidate;
        let policy = super::usage_policy::UsagePolicy::from_settings(&settings);
        let mut manifests = self.manifests.write().expect("manifests lock");
        let mut versions = self
            .connection_versions
            .write()
            .expect("connection versions lock");
        let mut providers = self.providers.write().expect("providers lock");
        providers.retain(|id, _| candidate.contains_key(id));
        versions.retain(|id, _| candidate.contains_key(id));
        for (id, m) in &candidate {
            if manifests.get(id) != Some(m) {
                providers.insert(id.clone(), provider_state_of(m));
                versions.insert(id.clone(), Arc::new(()));
                self.mark_dirty(id);
            }
        }
        *manifests = candidate;
        *self.usage_policy.write().expect("cost policy lock") = policy;
        *self.custom_ids.write().expect("custom ids lock") =
            settings.providers.iter().map(|p| p.id.clone()).collect();
        *self.model_groups.write().expect("groups lock") = settings.model_groups;
        *self.paused_providers.write().expect("paused lock") = settings.paused_providers;
        *self
            .restored_keychain_only
            .write()
            .expect("restored keychain lock") = settings.restored_keychain_only.clone();
        let mut keys = self.keys.write().expect("keys lock");
        keys.retain(|id, _| manifests.contains_key(id));
        for (id, key) in bound_keys {
            if keys.get(&id) != Some(&key) {
                versions.insert(id.clone(), Arc::new(()));
                if let Some(provider) = providers.get_mut(&id) {
                    provider.key_retry_at = None;
                }
                self.mark_dirty(&id);
            }
            keys.insert(id, key);
        }
        self.chatgpt
            .set_login_source(if settings.chatgpt_login_source.is_empty() {
                Some(self.chatgpt.detect_login_source())
            } else {
                crate::chatgpt::LoginSource::parse(&settings.chatgpt_login_source)
            });
    }
}
fn merge_view(target: &mut Value, fields: Value) {
    if let (Some(target), Some(fields)) = (target.as_object_mut(), fields.as_object()) {
        target.extend(fields.clone());
    }
}
fn merged(document: &Value, changes: &BTreeMap<String, Option<Value>>) -> Result<Value, String> {
    let mut result = document.clone();
    let object = result.as_object_mut().ok_or("配置需要对象")?;
    for (key, value) in changes {
        if let Some(value) = value {
            object.insert(key.clone(), value.clone());
        } else {
            object.remove(key);
        }
    }
    Ok(result)
}

fn merge_named_items(
    before: Option<&Value>,
    after: &Value,
    identity: &str,
    edited: &BTreeSet<String>,
) -> Result<Value, String> {
    let mut output = match before {
        Some(value) => value.as_array().ok_or("原配置集合格式无效")?.clone(),
        None => Vec::new(),
    };
    let mut seen = BTreeSet::new();
    for item in &output {
        let id = item[identity].as_str().ok_or("原配置条目标识缺失")?;
        if !seen.insert(id.to_owned()) {
            return Err("原配置条目标识重复".into());
        }
    }
    for item in after.as_array().ok_or("恢复集合格式无效")? {
        let id = item[identity].as_str().ok_or("恢复条目标识缺失")?;
        if !edited.contains(id) {
            continue;
        }
        if let Some(current) = output
            .iter_mut()
            .find(|old| old[identity].as_str() == Some(id))
        {
            let object = current.as_object_mut().ok_or("原配置条目格式无效")?;
            for (key, value) in item.as_object().ok_or("恢复条目格式无效")? {
                object.insert(key.clone(), value.clone());
            }
        } else {
            output.push(item.clone());
        }
    }
    Ok(Value::Array(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use credentials::{KeyStore, MemoryStore};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Fixture {
        dir: PathBuf,
        state: GatewayState,
        keys: Arc<MemoryStore>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    impl Fixture {
        fn new(manifests: BTreeMap<String, Manifest>, initial_keys: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "aubridge-backup-unit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.json"), r#"{"port":3427,"future":11}"#).unwrap();
            let keys = Arc::new(MemoryStore::default());
            for (id, key) in initial_keys {
                keys.set(id, key).unwrap();
            }
            let state = GatewayState::from_manifests(manifests, "fixture")
                .with_codex_channel(crate::chatgpt::ChatGptChannel::new(
                    dir.join("isolated-auth.json"),
                    dir.join("isolated-config.toml"),
                ))
                .with_settings(dir.join("config.json"), dir.join("stash"))
                .with_key_store(keys.clone());
            Self { dir, state, keys }
        }
        fn action(&self, action: &str, request: Value) -> Result<Value, String> {
            self.state
                .work_config_action(action, serde_json::from_value(request).unwrap())
        }
        fn stage(&self, payload: Payload) -> Value {
            let path = self.dir.join("backup.json");
            std::fs::write(&path, Archive::encode(payload).unwrap()).unwrap();
            let preview = self.action("preview", json!({"path":path})).unwrap();
            self.action("stage", json!({"preview_id":preview["preview_id"]}))
                .unwrap()
        }
    }
    fn payload(sources: Vec<Source>) -> Payload {
        Payload {
            sources,
            groups: vec![],
            model_selections: BTreeMap::new(),
            paused_providers: vec![],
            chatgpt_login_source: "codex".into(),
            client_wiring: vec![],
            cost_intent: None,
        }
    }
    fn custom_source(id: &str) -> Source {
        Source {
            id: id.into(),
            source: SourceKind::Custom,
            base: "https://fixture.invalid/v1".into(),
            auth: "bearer".into(),
            protocol: "chat".into(),
            kind: "free_tier".into(),
            models: vec![format!("{id}-model")],
        }
    }
    #[test]
    fn installed_k17_group_referencing_default_chatgpt_exports_without_provider_override() {
        let manifests = registry::presets()
            .into_iter()
            .map(|manifest| (manifest.id.clone(), manifest))
            .collect();
        let f = Fixture::new(manifests, &[]);
        f.state
            .upsert_model_group(&credentials::settings::ModelGroup {
                name: "k17-chatgpt".into(),
                members: vec!["gpt-6.1-sol".into()],
                rule: "free_first".into(),
            })
            .unwrap();
        let before = f.state.manifest("chatgpt").unwrap();
        let document = backup::read_document(&f.dir.join("config.json")).unwrap();
        assert!(document["providers"].as_array().is_some_and(Vec::is_empty));
        assert!(document["model_selections"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty));
        let destination = f.dir.with_extension("work-config.json");
        let result = f.action("export", json!({"path":destination})).unwrap();
        assert_eq!(result["status"], "exported");
        let archive = Archive::decode(&std::fs::read(&destination).unwrap()).unwrap();
        assert!(archive
            .payload
            .sources
            .iter()
            .any(|source| source.id == "chatgpt"
                && source.source == SourceKind::Preset
                && source.models.contains(&"gpt-6.1-sol".into())));
        assert_eq!(archive.payload.groups[0].members, vec!["gpt-6.1-sol"]);
        assert_eq!(f.state.manifest("chatgpt").unwrap(), before);
        std::fs::remove_file(destination).unwrap();
    }
    #[test]
    fn explicit_strict_cost_intent_survives_export_restore_and_restart() {
        use credentials::usage_policy::CostIntent;
        let origin = Fixture::new(BTreeMap::new(), &[]);
        origin
            .state
            .set_cost_policy(super::super::usage_policy::UsagePolicy {
                cost_intent: Some(CostIntent::ConfirmedFreeOnly),
                ..Default::default()
            })
            .unwrap();
        let archived = origin
            .state
            .backup_payload(&backup::read_document(&origin.dir.join("config.json")).unwrap())
            .unwrap();
        assert_eq!(
            serde_json::to_value(&archived).unwrap()["cost_intent"],
            "confirmed_free_only"
        );
        let vendor = custom_manifest(&custom(&custom_source("paid-fixture"))).unwrap();
        let target = Fixture::new(BTreeMap::from([(vendor.id.clone(), vendor.clone())]), &[]);
        let staged = target.stage(archived);
        target.action("promote",json!({"restore_id":staged["restore_id"],"target_revision":staged["target_revision"]})).unwrap();
        assert_eq!(
            target.state.cost_policy().cost_intent,
            Some(CostIntent::ConfirmedFreeOnly)
        );
        assert!(target
            .state
            .cost_block(&vendor, &vendor.free_models[0], 0)
            .is_some());
        let restarted = GatewayState::from_manifests(
            BTreeMap::from([(vendor.id.clone(), vendor.clone())]),
            "fixture",
        )
        .with_codex_channel(crate::chatgpt::ChatGptChannel::new(
            target.dir.join("isolated-auth.json"),
            target.dir.join("isolated-config.toml"),
        ))
        .with_settings(target.dir.join("config.json"), target.dir.join("stash"));
        assert_eq!(
            restarted.cost_policy().cost_intent,
            Some(CostIntent::ConfirmedFreeOnly)
        );
        assert!(restarted
            .cost_block(&vendor, &vendor.free_models[0], 0)
            .is_some());
    }
    #[test]
    fn legacy_archive_preserves_explicit_cost_policy_and_existing_confirmation_marker() {
        let f = Fixture::new(BTreeMap::new(), &[]);
        f.state
            .set_cost_policy(super::super::usage_policy::UsagePolicy {
                cost_intent: Some(CostIntent::ConfirmedFreeOnly),
                ..Default::default()
            })
            .unwrap();
        let config = f.dir.join("config.json");
        let mut original = backup::read_document(&config).unwrap();
        original["restored_desired_cost_intent"] = json!(CostIntent::FreeThenSubscription);
        std::fs::write(&config, serde_json::to_vec(&original).unwrap()).unwrap();
        let staged = f.stage(payload(vec![]));
        let receipt=f.action("promote",json!({"restore_id":staged["restore_id"],"target_revision":staged["target_revision"]})).unwrap();
        let document = backup::read_document(&config).unwrap();
        assert_eq!(document["cost_intent"], "confirmed_free_only");
        assert_eq!(
            document["restored_desired_cost_intent"],
            "free_then_subscription"
        );
        assert_eq!(receipt["cost_intent_recorded"], false);
        f.action("undo", json!({"restore_id":staged["restore_id"]}))
            .unwrap();
        assert_eq!(
            f.state.cost_policy().cost_intent,
            Some(CostIntent::ConfirmedFreeOnly)
        );
        assert_eq!(
            backup::read_document(&config).unwrap()["restored_desired_cost_intent"],
            "free_then_subscription"
        );
    }
    #[test]
    fn subscription_intent_restores_strict_until_explicit_d52_confirmation_and_undo_respects_that_edit(
    ) {
        use credentials::usage_policy::{
            connection_identity, SubscriptionAuthorization, SubscriptionBasis,
        };
        let provider = registry::kimi_code();
        let f = Fixture::new(
            BTreeMap::from([(provider.id.clone(), provider.clone())]),
            &[],
        );
        let mut archived = payload(vec![]);
        archived.cost_intent = Some(CostIntent::FreeThenSubscription);
        let staged = f.stage(archived);
        let receipt=f.action("promote",json!({"restore_id":staged["restore_id"],"target_revision":staged["target_revision"]})).unwrap();
        assert_eq!(receipt["requested_cost_intent"], "free_then_subscription");
        assert_eq!(receipt["effective_cost_intent"], "confirmed_free_only");
        assert_eq!(receipt["needs_cost_confirmation"], true);
        assert!(f.state.cost_policy().subscription_authorizations.is_empty());
        assert!(f
            .state
            .cost_block(&provider, &provider.free_models[0], 0)
            .is_some());
        let restarted = GatewayState::from_manifests(
            BTreeMap::from([(provider.id.clone(), provider.clone())]),
            "fixture",
        )
        .with_codex_channel(crate::chatgpt::ChatGptChannel::new(
            f.dir.join("isolated-auth.json"),
            f.dir.join("isolated-config.toml"),
        ))
        .with_settings(f.dir.join("config.json"), f.dir.join("stash"));
        let status = restarted
            .work_config_action("status", Request::default())
            .unwrap();
        assert_eq!(status["requested_cost_intent"], "free_then_subscription");
        assert_eq!(status["effective_cost_intent"], "confirmed_free_only");
        assert_eq!(status["needs_cost_confirmation"], true);
        assert!(status["cost_intent_recorded"].is_null());
        f.state
            .set_cost_policy(super::super::usage_policy::UsagePolicy {
                cost_intent: Some(CostIntent::FreeThenSubscription),
                subscription_authorizations: vec![SubscriptionAuthorization {
                    provider: provider.id.clone(),
                    basis: SubscriptionBasis::UserDeclared,
                    connection_identity: connection_identity(&provider),
                }],
                configuration_issue: None,
            })
            .unwrap();
        let config = backup::read_document(&f.dir.join("config.json")).unwrap();
        assert!(config.get("restored_desired_cost_intent").is_none());
        assert!(f
            .state
            .cost_block(&provider, &provider.free_models[0], 0)
            .is_none());
        assert!(f
            .action("undo", json!({"restore_id":staged["restore_id"]}))
            .is_err());
        assert_eq!(
            f.state.cost_policy().cost_intent,
            Some(CostIntent::FreeThenSubscription)
        );
    }
    #[test]
    fn status_projects_persisted_wiring_intents_after_pending_is_cleared() {
        let f = Fixture::new(BTreeMap::new(), &[]);
        let config = f.dir.join("config.json");
        let mut document = backup::read_document(&config).unwrap();
        document["restored_client_wiring_intents"] = json!([WiringIntent {
            agent: "codex".into(),
            enabled: true,
            model: "aubridge".into()
        }]);
        std::fs::write(&config, serde_json::to_vec(&document).unwrap()).unwrap();
        let status = f.action("status", json!({})).unwrap();
        assert!(status["pending"].is_null());
        assert_eq!(
            status["restored_client_wiring_intents"],
            document["restored_client_wiring_intents"]
        );
        assert_eq!(status["client_wiring_status"], "intent_only");
    }
    #[test]
    fn same_manifest_key_rotation_invalidates_old_callbacks_and_clears_key_backoff() {
        let original = registry::kimi_code();
        let id = original.id.clone();
        let source = source(&original, SourceKind::Preset);
        let f = Fixture::new(
            BTreeMap::from([(id.clone(), original)]),
            &[(&id, "old-fixture-key")],
        );
        let old = f.state.connection_versions.read().unwrap()[&id].clone();
        f.state
            .providers
            .write()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .key_retry_at = Some(u64::MAX);
        f.keys.set(&id, "new-fixture-key").unwrap();
        let staged = f.stage(payload(vec![source.clone()]));
        let bound=f.action("rebind",json!({"restore_id":staged["restore_id"],"source_id":id,"expected_identity":source.identity().unwrap(),"target_revision":staged["target_revision"],"confirm_endpoint":true})).unwrap();
        f.action(
            "promote",
            json!({"restore_id":staged["restore_id"],"target_revision":bound["target_revision"]}),
        )
        .unwrap();
        let called = std::cell::Cell::new(false);
        f.state
            .update_current_connection(&id, Some(&old), || called.set(true));
        assert!(!called.get());
        let new = f.state.connection_versions.read().unwrap()[&id].clone();
        f.state
            .update_current_connection(&id, Some(&new), || called.set(true));
        assert!(called.get());
        assert_eq!(f.state.providers.read().unwrap()[&id].key_retry_at, None);
        assert_eq!(f.state.api_key_opt(&id).as_deref(), Some("new-fixture-key"));
    }
    #[test]
    fn promote_rejects_external_config_edit_during_keychain_wait() {
        struct WaitingStore {
            inner: Arc<MemoryStore>,
            entered: Arc<std::sync::Barrier>,
            release: Arc<std::sync::Barrier>,
        }
        impl KeyStore for WaitingStore {
            fn get(&self, id: &str) -> Option<String> {
                self.entered.wait();
                self.release.wait();
                self.inner.get(id)
            }
            fn set(&self, _: &str, _: &str) -> Result<(), String> {
                Err("not used".into())
            }
            fn delete(&self, _: &str) -> Result<(), String> {
                Err("not used".into())
            }
        }
        let source = custom_source("waiting-fixture");
        let mut f = Fixture::new(BTreeMap::new(), &[(&source.id, "fixture-key")]);
        let staged = f.stage(payload(vec![source.clone()]));
        let bound=f.action("rebind",json!({"restore_id":staged["restore_id"],"source_id":source.id,"expected_identity":source.identity().unwrap(),"target_revision":staged["target_revision"],"confirm_endpoint":true})).unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        f.state.key_store = Some(Arc::new(WaitingStore {
            inner: f.keys.clone(),
            entered: entered.clone(),
            release: release.clone(),
        }));
        std::thread::scope(|scope| {
            let handle=scope.spawn(||f.action("promote",json!({"restore_id":staged["restore_id"],"target_revision":bound["target_revision"]})));
            entered.wait();
            let config = f.dir.join("config.json");
            let mut document = backup::read_document(&config).unwrap();
            document["providers"] = json!([custom(&custom_source("external-fixture"))]);
            std::fs::write(&config, serde_json::to_vec(&document).unwrap()).unwrap();
            release.wait();
            assert!(handle.join().unwrap().is_err());
            assert_eq!(backup::read_document(&config).unwrap(), document);
            assert!(f.state.manifest(&source.id).is_none());
        });
    }
    #[test]
    fn restoring_named_items_preserves_target_unique_and_unknown_fields() {
        let before = json!([{"id":"unique","models":["old"],"future":{"keep":true}},{"id":"edited","models":["before"],"future":17}]);
        let after = json!([{"id":"unique","models":["old"]},{"id":"edited","models":["after"]}]);
        let merged = merge_named_items(
            Some(&before),
            &after,
            "id",
            &BTreeSet::from(["edited".into()]),
        )
        .unwrap();
        assert_eq!(merged[0], before[0]);
        assert_eq!(merged[1]["future"], 17);
        assert_eq!(merged[1]["models"], json!(["after"]));
    }
    #[test]
    fn unrelated_provider_runtime_metadata_survives_restore_candidate() {
        let mut existing = registry::kimi_code();
        existing
            .context_windows
            .insert(existing.free_models[0].clone(), 123456);
        let f = Fixture::new(
            BTreeMap::from([(existing.id.clone(), existing.clone())]),
            &[],
        );
        let mut target = backup::read_document(&f.dir.join("config.json")).unwrap();
        target["providers"] = json!([custom(&custom_source("new-fixture"))]);
        let candidate = f.state.restore_runtime_candidate(&target).unwrap();
        assert_eq!(candidate.manifests.get(&existing.id), Some(&existing));
    }
    #[test]
    fn preset_selection_undo_restores_runtime_without_restart_and_uses_verified_key_snapshot() {
        let original = registry::kimi_code();
        let id = original.id.clone();
        let f = Fixture::new(
            BTreeMap::from([(id.clone(), original.clone())]),
            &[(&id, "old-fixture-key")],
        );
        f.keys.set(&id, "rotated-fixture-key").unwrap();
        let mut source = source(&original, SourceKind::Preset);
        source.models = vec!["restored-fixture-model".into()];
        let mut payload = payload(vec![source.clone()]);
        payload
            .model_selections
            .insert(id.clone(), source.models.clone());
        let staged = f.stage(payload);
        let bound=f.action("rebind",json!({"restore_id":staged["restore_id"],"source_id":id,"expected_identity":source.identity().unwrap(),"target_revision":staged["target_revision"],"confirm_endpoint":true})).unwrap();
        let promoted=f.action("promote",json!({"restore_id":staged["restore_id"],"target_revision":bound["target_revision"]})).unwrap();
        assert_eq!(f.state.manifest(&id).unwrap().free_models, source.models);
        assert_eq!(
            f.state.api_key_opt(&id).as_deref(),
            Some("rotated-fixture-key")
        );
        assert!(f.state.restored_keychain_only.read().unwrap().contains(&id));
        f.action("undo", json!({"restore_id":promoted["restore_id"]}))
            .unwrap();
        assert_eq!(
            f.state.manifest(&id).unwrap().free_models,
            original.free_models
        );
    }
    #[test]
    fn known_noncustom_id_conflict_rejects_before_touching_shared_keychain() {
        let source = custom_source("plugin-fixture");
        let manifest = custom_manifest(&custom(&source)).unwrap();
        let f = Fixture::new(
            BTreeMap::from([(source.id.clone(), manifest)]),
            &[(&source.id, "original-plugin-key")],
        );
        let path = f.dir.join("backup.json");
        std::fs::write(
            &path,
            Archive::encode(payload(vec![source.clone()])).unwrap(),
        )
        .unwrap();
        assert!(f
            .action("preview", json!({"path":path}))
            .unwrap_err()
            .contains("归属冲突"));
        assert_eq!(
            f.keys.get(&source.id).as_deref(),
            Some("original-plugin-key")
        );
    }
    #[test]
    fn disabled_installed_plugin_reserves_keychain_id_before_any_credential_io() {
        struct CountStore {
            inner: Arc<MemoryStore>,
            reads: std::sync::atomic::AtomicU64,
            writes: std::sync::atomic::AtomicU64,
        }
        impl KeyStore for CountStore {
            fn get(&self, id: &str) -> Option<String> {
                self.reads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.inner.get(id)
            }
            fn set(&self, id: &str, key: &str) -> Result<(), String> {
                self.writes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.inner.set(id, key)
            }
            fn delete(&self, _: &str) -> Result<(), String> {
                Err("not used".into())
            }
        }
        let source = custom_source("disabled-plugin");
        let mut f = Fixture::new(
            BTreeMap::new(),
            &[(&source.id, "original-disabled-plugin-key")],
        );
        let plugin_dir = f.dir.join("plugins");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("disabled.json"),
            serde_json::to_vec(&custom_manifest(&custom(&source)).unwrap()).unwrap(),
        )
        .unwrap();
        f.state.plugins_dir = Some(plugin_dir);
        let store = Arc::new(CountStore {
            inner: f.keys.clone(),
            reads: Default::default(),
            writes: Default::default(),
        });
        f.state.key_store = Some(store.clone());
        let path = f.dir.join("backup.json");
        std::fs::write(
            &path,
            Archive::encode(payload(vec![source.clone()])).unwrap(),
        )
        .unwrap();
        assert!(f
            .action("preview", json!({"path":path}))
            .unwrap_err()
            .contains("已安装插件"));
        assert_eq!(store.reads.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(store.writes.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(
            f.keys.get(&source.id).as_deref(),
            Some("original-disabled-plugin-key")
        );
    }
    #[test]
    fn before_image_write_failure_leaves_config_and_runtime_unchanged() {
        let source = custom_source("write-failure-fixture");
        let f = Fixture::new(BTreeMap::new(), &[(&source.id, "fake-fixture-key")]);
        let staged = f.stage(payload(vec![source.clone()]));
        let bound=f.action("rebind",json!({"restore_id":staged["restore_id"],"source_id":source.id,"expected_identity":source.identity().unwrap(),"target_revision":staged["target_revision"],"confirm_endpoint":true})).unwrap();
        let config = f.dir.join("config.json");
        let before = std::fs::read(&config).unwrap();
        let journal = f.dir.join("work-config-restore-before.json");
        std::fs::remove_file(&journal).unwrap();
        std::fs::create_dir(&journal).unwrap();
        assert!(f.action("promote",json!({"restore_id":staged["restore_id"],"target_revision":bound["target_revision"]})).is_err());
        assert_eq!(std::fs::read(config).unwrap(), before);
        assert!(f.state.manifest(&source.id).is_none());
    }
    #[test]
    fn missing_key_can_be_supplied_without_premature_activation_and_paused_intent_does_not_block_next_restore(
    ) {
        let active = custom_source("active-fixture");
        let dormant = custom_source("dormant-fixture");
        let f = Fixture::new(BTreeMap::new(), &[]);
        let mut payload = payload(vec![active.clone(), dormant.clone()]);
        payload.paused_providers.push(dormant.id.clone());
        let staged = f.stage(payload);
        let bound=f.action("supply-key",json!({"restore_id":staged["restore_id"],"source_id":active.id,"expected_identity":active.identity().unwrap(),"target_revision":staged["target_revision"],"confirm_endpoint":true,"confirm_key_write":true,"key":"supplied-fixture-key"})).unwrap();
        assert!(f.state.manifest(&active.id).is_none());
        assert!(f.state.manifest(&dormant.id).is_none());
        f.action(
            "promote",
            json!({"restore_id":staged["restore_id"],"target_revision":bound["target_revision"]}),
        )
        .unwrap();
        assert!(f.state.manifest(&active.id).is_some());
        assert!(f.state.manifest(&dormant.id).is_none());
        assert!(f.keys.get(&dormant.id).is_none());
        let status = f.action("status", json!({})).unwrap();
        assert!(status["pending"].is_null());
        assert_eq!(status["dormant"].as_array().unwrap().len(), 1);
        let document = backup::read_document(&f.dir.join("config.json")).unwrap();
        let reexport = Archive::encode(f.state.backup_payload(&document).unwrap()).unwrap();
        assert!(Archive::decode(&reexport).is_ok());
        let second = custom_source("next-fixture");
        let path = f.dir.join("second.json");
        std::fs::write(
            &path,
            Archive::encode(super::tests::payload(vec![second])).unwrap(),
        )
        .unwrap();
        assert!(f.action("preview", json!({"path":path})).is_ok());
    }
}
