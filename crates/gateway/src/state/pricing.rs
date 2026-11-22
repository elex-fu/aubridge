//! Price edits share the settings lock and preserve unrelated settings fields.
use super::*;
use manifest::model::BenchmarkMapping;

impl GatewayState {
    pub(crate) fn served_price_snapshot(
        &self,
        manifest: &Manifest,
        model: &str,
    ) -> Option<ledger::PriceSnapshot> {
        // Declarative manifests (including plugins) cannot authenticate provenance.
        let directory = self.directory_view(manifest);
        let trusted = (!directory.stale)
            .then(|| directory.models.iter().find(|row| row.id == model))
            .flatten();
        if let Some(row) = trusted {
            if let Some(mut snapshot) = ledger::PriceSnapshot::from_facts(model, &row.facts) {
                snapshot.provider = manifest.id.clone();
                snapshot.basis = "provider-directory".into();
                snapshot.source = if manifest.id == "openrouter" {
                    self.openrouter_catalog.as_ref()?.price_source().into()
                } else {
                    directory::price_source(manifest)
                };
                snapshot.observed_at = directory.fetched_at;
                snapshot.version = directory.fetched_at.map(|at| format!("directory-{at}"));
                return Some(snapshot);
            }
        }
        let declared = self.manifests.read().ok()?.get(&manifest.id).cloned();
        declared
            .as_ref()
            .and_then(|manifest| manifest.model_facts.get(model))
            .and_then(|facts| ledger::PriceSnapshot::from_facts(model, facts))
            .map(|mut snapshot| {
                snapshot.provider = manifest.id.clone();
                snapshot.basis = "local-declaration".into();
                snapshot.source = "local-manifest".into();
                snapshot.version = declared.as_ref().map(|manifest| manifest.version.clone());
                snapshot
            })
            .or_else(|| {
                ledger::PriceSnapshot::builtin(model).map(|mut snapshot| {
                    snapshot.provider = manifest.id.clone();
                    snapshot
                })
            })
    }

    pub(crate) fn pricing_mappings(&self) -> Result<Vec<BenchmarkMapping>, String> {
        let (path, _) = self
            .settings
            .as_ref()
            .ok_or("settings storage unavailable")?;
        let _guard = self
            .settings_io
            .lock()
            .map_err(|_| "settings lock unavailable")?;
        let mappings = settings_for_edit(path)?.benchmark_mappings;
        validate(&mappings)?;
        Ok(mappings)
    }

    pub(crate) fn set_pricing_mappings(
        &self,
        expected: &[BenchmarkMapping],
        mappings: Vec<BenchmarkMapping>,
    ) -> Result<(), String> {
        validate(&mappings)?;
        let (path, _) = self
            .settings
            .as_ref()
            .ok_or("settings storage unavailable")?;
        let _guard = self
            .settings_io
            .lock()
            .map_err(|_| "settings lock unavailable")?;
        let mut settings = settings_for_edit(path)?;
        if settings.benchmark_mappings != expected {
            return Err("pricing changed; reload before saving".into());
        }
        settings.benchmark_mappings = mappings;
        settings.save(path).map_err(|e| e.to_string())
    }

    pub(crate) fn benchmark_snapshot(
        &self,
        provider: &str,
        model: &str,
    ) -> Option<ledger::PriceSnapshot> {
        let (path, _) = self.settings.as_ref()?;
        let _guard = self.settings_io.lock().ok()?;
        let mappings = settings_for_edit(path).ok()?.benchmark_mappings;
        // Request path does not resolve aliases or traverse the configuration graph.
        // Full validation belongs to admin reads/writes; a hand-edited duplicate is unknown.
        if mappings.len() > 512 {
            return None;
        }
        let mut matching = mappings
            .iter()
            .filter(|m| m.provider == provider && m.model == model);
        let mapping = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        ledger::PriceSnapshot::benchmark(mapping)
    }
}

fn validate(mappings: &[BenchmarkMapping]) -> Result<(), String> {
    if mappings.len() > 512 {
        return Err("too many benchmark mappings".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for m in mappings {
        if !m.valid() || !seen.insert((&m.provider, &m.model)) {
            return Err("invalid or duplicate benchmark mapping; prices must be finite nonnegative USD per million tokens".into());
        }
        // Targets are labels with explicit prices, never recursively resolved aliases.
        // Reject cycles anyway so later extensions cannot reinterpret ambiguous evidence.
        let mut path = std::collections::BTreeSet::new();
        let mut next = Some(m);
        while let Some(current) = next {
            if !path.insert((&current.provider, &current.model)) {
                return Err("benchmark mapping cycle".into());
            }
            next = mappings
                .iter()
                .find(|n| n.provider == current.target_provider && n.model == current.target_model);
            if next.is_some_and(|n| std::ptr::eq(n, current)) {
                break;
            } // own explicit price is valid
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mapping() -> BenchmarkMapping {
        BenchmarkMapping {
            provider: "chatgpt".into(),
            model: "gpt-6.1-sol".into(),
            target_provider: "user-choice".into(),
            target_model: "benchmark".into(),
            source: "explicit test input, not official data".into(),
            version: "v1".into(),
            observed_at: 1,
            price: manifest::model::ModelPrice {
                input_per_million: 2.0,
                output_per_million: 10.0,
                ..Default::default()
            },
        }
    }
    #[test]
    fn edits_merge_fields_reject_stale_writers_and_read_bad_config_closed() {
        let root = std::env::temp_dir().join(format!(
            "aubridge-pricing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.json");
        std::fs::write(
            &path,
            r#"{"port":4321,"future":{"keep":true},"share_login":false}"#,
        )
        .unwrap();
        let state = GatewayState::from_manifests(Default::default(), "test")
            .with_settings(path.clone(), root.join("stash"));
        assert_eq!(state.pricing_mappings().unwrap(), vec![]);
        let m = mapping();
        state.set_pricing_mappings(&[], vec![m.clone()]).unwrap();
        let snapshot = state.benchmark_snapshot("chatgpt", "gpt-6.1-sol").unwrap();
        assert_eq!(snapshot.basis, "user-benchmark");
        assert_eq!(snapshot.target_model.as_deref(), Some("benchmark"));
        assert!(state.set_pricing_mappings(&[], vec![]).is_err());
        let mut changed = m.clone();
        changed.price.input_per_million = 99.0;
        changed.version = "v2".into();
        state.set_pricing_mappings(&[m], vec![changed]).unwrap();
        assert_eq!(snapshot.price.input_per_million, 2.0);
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(document["port"], 4321);
        assert_eq!(document["future"]["keep"], true);
        assert_eq!(document["share_login"], false);
        std::fs::write(&path, "broken").unwrap();
        assert!(state.pricing_mappings().is_err());
        assert!(state.benchmark_snapshot("chatgpt", "gpt-6.1-sol").is_none());
        assert!(state.set_pricing_mappings(&[], vec![]).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn declarative_vendor_claim_cannot_create_directory_price() {
        let manifest: Manifest = serde_json::from_value(serde_json::json!({
            "id":"untrusted", "version":"1", "provider":{"base":"https://fake.example/v1","auth":"bearer"},
            "free_models":["m"], "quota":{"source":"inferred","rest_hint":"daily-utc"},
            "permissions":{"net":["fake.example"],"store_key":false},
            "model_facts":{"m":{"source":"vendor-directory","observed_at":9999999999_u64,"price":{"input_per_million":1,"output_per_million":2}}}
        })).unwrap();
        let state = GatewayState::from_manifests(
            std::collections::BTreeMap::from([(manifest.id.clone(), manifest.clone())]),
            "test",
        );
        let snapshot = state.served_price_snapshot(&manifest, "m").unwrap();
        assert_eq!(snapshot.basis, "local-declaration");
        assert_eq!(snapshot.source, "local-manifest");
    }
    #[test]
    fn invalid_prices_duplicate_scopes_and_cycles_are_rejected() {
        let a = mapping();
        let mut b = a.clone();
        b.price.input_per_million = f64::NAN;
        assert!(validate(&[b]).is_err());
        assert!(validate(&[a.clone(), a.clone()]).is_err());
        let mut b = a.clone();
        b.provider = a.target_provider.clone();
        b.model = a.target_model.clone();
        b.target_provider = a.provider.clone();
        b.target_model = a.model.clone();
        assert!(validate(&[a, b]).is_err());
    }
}
