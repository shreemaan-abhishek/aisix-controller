//! Custom resources + Secrets -> resources-file entries.
//!
//! Each CR becomes one entry of the gateway's `resources.yaml` collection for
//! its kind. Reference fields are resolved here; everything else passes
//! through untouched for the gateway pipeline to validate.

use std::collections::BTreeMap;

use aisix_core::models::ApiKey;
use k8s_openapi::api::core::v1::Secret;
use serde_json::{Map, Value};

use crate::crd::{Kind, API_KEY_SECRET_REF, IDENTITY_FIELD, KEY_SECRET_REF, PROVIDER_KEY_REF};

/// One custom resource, reduced to what rendering needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub kind: Kind,
    pub name: String,
    pub generation: Option<i64>,
    /// Sort key for duplicate identities: the older resource wins.
    pub created: Option<String>,
    pub fields: Map<String, Value>,
}

impl Source {
    pub fn key(&self) -> SourceKey {
        SourceKey {
            kind: self.kind,
            name: self.name.clone(),
        }
    }

    /// AISIX identity: `display_name`, defaulting to `metadata.name`.
    pub fn identity(&self) -> String {
        self.fields
            .get(IDENTITY_FIELD)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| self.name.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceKey {
    pub kind: Kind,
    pub name: String,
}

impl std::fmt::Display for SourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.kind.name(), self.name)
    }
}

/// Secret data by Secret name, then key.
pub type Secrets = BTreeMap<String, BTreeMap<String, Vec<u8>>>;

pub fn secrets_from(list: &[Secret]) -> Secrets {
    list.iter()
        .filter_map(|s| {
            let name = s.metadata.name.clone()?;
            let data = s
                .data
                .as_ref()
                .map(|d| d.iter().map(|(k, v)| (k.clone(), v.0.clone())).collect())
                .unwrap_or_default();
            Some((name, data))
        })
        .collect()
}

/// A rendered entry, already escaped for `${VAR}` interpolation.
pub type Entry = Value;

/// Render every source. Sources come back in file order: by kind, then by
/// creation time and name.
pub fn render_all(sources: &[Source], secrets: &Secrets) -> Vec<(Source, Result<Entry, String>)> {
    let mut ordered: Vec<Source> = sources.to_vec();
    ordered.sort_by(|a, b| (a.kind, &a.created, &a.name).cmp(&(b.kind, &b.created, &b.name)));

    let provider_identities: BTreeMap<String, String> = ordered
        .iter()
        .filter(|s| s.kind == Kind::ProviderKey)
        .map(|s| (s.name.clone(), s.identity()))
        .collect();

    let mut seen: BTreeMap<(Kind, String), String> = BTreeMap::new();
    ordered
        .into_iter()
        .map(|s| {
            let result = match seen.get(&(s.kind, s.identity())) {
                Some(owner) => Err(format!(
                    "{IDENTITY_FIELD} {:?} is already used by {}/{owner}",
                    s.identity(),
                    s.kind.name()
                )),
                None => {
                    seen.insert((s.kind, s.identity()), s.name.clone());
                    render_one(&s, secrets, &provider_identities)
                }
            };
            (s, result)
        })
        .collect()
}

fn render_one(
    source: &Source,
    secrets: &Secrets,
    provider_identities: &BTreeMap<String, String>,
) -> Result<Entry, String> {
    let mut fields = source.fields.clone();
    fields.insert(IDENTITY_FIELD.into(), Value::String(source.identity()));

    match source.kind {
        Kind::ProviderKey => {
            for inline in ["api_key", "secret"] {
                if fields.contains_key(inline) {
                    return Err(format!(
                        "set the credential through {API_KEY_SECRET_REF}, not `{inline}`"
                    ));
                }
            }
            let reference = fields
                .remove(API_KEY_SECRET_REF)
                .ok_or_else(|| format!("{API_KEY_SECRET_REF} is required"))?;
            let value = secret_value(&reference, secrets, API_KEY_SECRET_REF)?;
            fields.insert("api_key".into(), Value::String(value));
        }
        Kind::Model => {
            if fields.contains_key("provider_key_id") {
                return Err(format!(
                    "reference the provider key through {PROVIDER_KEY_REF}"
                ));
            }
            if let Some(reference) = fields.remove(PROVIDER_KEY_REF) {
                let name = reference
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{PROVIDER_KEY_REF}.name is required"))?;
                let identity = provider_identities.get(name).ok_or_else(|| {
                    format!("{PROVIDER_KEY_REF} names ProviderKey {name:?}, which does not exist")
                })?;
                fields.insert("provider_key".into(), Value::String(identity.clone()));
            }
        }
        Kind::APIKey => {
            if fields.contains_key("key_hash") {
                return Err(format!(
                    "set the key through {KEY_SECRET_REF}, not `key_hash`"
                ));
            }
            let reference = fields
                .remove(KEY_SECRET_REF)
                .ok_or_else(|| format!("{KEY_SECRET_REF} is required"))?;
            let plaintext = secret_value(&reference, secrets, KEY_SECRET_REF)?;
            fields.insert(
                "key_hash".into(),
                Value::String(ApiKey::hash_bearer(&plaintext)),
            );
        }
    }

    let mut entry = Value::Object(fields);
    escape_dollars(&mut entry);
    Ok(entry)
}

fn secret_value(reference: &Value, secrets: &Secrets, field: &str) -> Result<String, String> {
    let name = reference
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field}.name is required"))?;
    let key = reference
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field}.key is required"))?;
    let data = secrets
        .get(name)
        .ok_or_else(|| format!("{field}: Secret {name:?} not found"))?;
    let bytes = data
        .get(key)
        .ok_or_else(|| format!("{field}: Secret {name:?} has no key {key:?}"))?;
    let value = String::from_utf8(bytes.clone())
        .map_err(|_| format!("{field}: Secret {name:?} key {key:?} is not UTF-8"))?;
    let value = value.trim_end_matches(['\r', '\n']).to_string();
    if value.is_empty() {
        return Err(format!("{field}: Secret {name:?} key {key:?} is empty"));
    }
    Ok(value)
}

/// The resources file interpolates `${VAR}` in every string scalar. Values
/// from custom resources are literal, so every `$` is written as `$$`.
pub fn escape_dollars(value: &mut Value) {
    match value {
        Value::String(s) if s.contains('$') => *s = s.replace('$', "$$"),
        Value::Array(items) => items.iter_mut().for_each(escape_dollars),
        Value::Object(map) => map.values_mut().for_each(escape_dollars),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source(kind: Kind, name: &str, fields: Value) -> Source {
        Source {
            kind,
            name: name.into(),
            generation: Some(1),
            created: Some("2026-01-01T00:00:00Z".into()),
            fields: fields.as_object().unwrap().clone(),
        }
    }

    fn secrets() -> Secrets {
        let mut s = Secrets::new();
        s.insert(
            "creds".into(),
            BTreeMap::from([
                ("openai".into(), b"sk-$abc\n".to_vec()),
                ("caller".into(), b"caller-key".to_vec()),
                ("empty".into(), Vec::new()),
            ]),
        );
        s
    }

    fn rendered(sources: &[Source]) -> BTreeMap<String, Result<Value, String>> {
        render_all(sources, &secrets())
            .into_iter()
            .map(|(s, r)| (s.key().to_string(), r))
            .collect()
    }

    #[test]
    fn provider_key_takes_credential_from_secret_and_escapes_it() {
        let out = rendered(&[source(
            Kind::ProviderKey,
            "openai",
            json!({"provider": "openai", "api_base": "http://x/${not_env}", "api_key_secret_ref": {"name": "creds", "key": "openai"}}),
        )]);
        let entry = out["ProviderKey/openai"].as_ref().unwrap();
        assert_eq!(entry["api_key"], json!("sk-$$abc"));
        assert_eq!(entry["api_base"], json!("http://x/$${not_env}"));
        assert_eq!(entry["display_name"], json!("openai"));
        assert!(entry.get(API_KEY_SECRET_REF).is_none());
    }

    #[test]
    fn model_reference_resolves_to_provider_identity() {
        let out = rendered(&[
            source(
                Kind::ProviderKey,
                "pk",
                json!({"display_name": "Main OpenAI", "api_key_secret_ref": {"name": "creds", "key": "openai"}}),
            ),
            source(
                Kind::Model,
                "gpt",
                json!({"display_name": "gpt-4o", "provider": "openai", "model_name": "gpt-4o", "provider_key_ref": {"name": "pk"}}),
            ),
        ]);
        let entry = out["Model/gpt"].as_ref().unwrap();
        assert_eq!(entry["provider_key"], json!("Main OpenAI"));
        assert_eq!(entry["display_name"], json!("gpt-4o"));
        assert!(entry.get(PROVIDER_KEY_REF).is_none());
    }

    #[test]
    fn api_key_is_hashed() {
        let out = rendered(&[source(
            Kind::APIKey,
            "caller",
            json!({"allowed_models": ["*"], "key_secret_ref": {"name": "creds", "key": "caller"}}),
        )]);
        let entry = out["APIKey/caller"].as_ref().unwrap();
        assert_eq!(entry["key_hash"], json!(ApiKey::hash_bearer("caller-key")));
        assert_eq!(entry["key_hash"].as_str().unwrap().len(), 64);
        assert!(!entry.to_string().contains("caller-key"));
    }

    #[test]
    fn missing_references_are_errors() {
        let out = rendered(&[
            source(
                Kind::ProviderKey,
                "a",
                json!({"api_key_secret_ref": {"name": "nope", "key": "k"}}),
            ),
            source(
                Kind::ProviderKey,
                "b",
                json!({"api_key_secret_ref": {"name": "creds", "key": "nope"}}),
            ),
            source(
                Kind::ProviderKey,
                "c",
                json!({"api_key_secret_ref": {"name": "creds", "key": "empty"}}),
            ),
            source(
                Kind::Model,
                "m",
                json!({"provider_key_ref": {"name": "ghost"}}),
            ),
            source(Kind::APIKey, "k", json!({})),
        ]);
        assert!(out["ProviderKey/a"]
            .as_ref()
            .unwrap_err()
            .contains("not found"));
        assert!(out["ProviderKey/b"]
            .as_ref()
            .unwrap_err()
            .contains("has no key"));
        assert!(out["ProviderKey/c"].as_ref().unwrap_err().contains("empty"));
        assert!(out["Model/m"]
            .as_ref()
            .unwrap_err()
            .contains("does not exist"));
        assert!(out["APIKey/k"].as_ref().unwrap_err().contains("required"));
    }

    #[test]
    fn inline_credentials_are_refused() {
        let out = rendered(&[source(
            Kind::ProviderKey,
            "a",
            json!({"api_key": "sk", "api_key_secret_ref": {"name": "creds", "key": "openai"}}),
        )]);
        assert!(out["ProviderKey/a"]
            .as_ref()
            .unwrap_err()
            .contains(API_KEY_SECRET_REF));
    }

    #[test]
    fn duplicate_identity_loses_to_older_resource() {
        let mut older = source(Kind::Model, "zeta", json!({"display_name": "same"}));
        older.created = Some("2025-01-01T00:00:00Z".into());
        let newer = source(Kind::Model, "alpha", json!({"display_name": "same"}));
        let out = rendered(&[newer, older]);
        assert!(out["Model/zeta"].is_ok());
        assert!(out["Model/alpha"]
            .as_ref()
            .unwrap_err()
            .contains("already used by Model/zeta"));
    }

    #[test]
    fn order_is_kind_then_age_then_name() {
        let out = render_all(
            &[
                source(
                    Kind::APIKey,
                    "k",
                    json!({"key_secret_ref": {"name": "creds", "key": "caller"}}),
                ),
                source(Kind::Model, "b", json!({})),
                source(Kind::Model, "a", json!({})),
                source(
                    Kind::ProviderKey,
                    "p",
                    json!({"api_key_secret_ref": {"name": "creds", "key": "openai"}}),
                ),
            ],
            &secrets(),
        );
        let names: Vec<String> = out.iter().map(|(s, _)| s.key().to_string()).collect();
        assert_eq!(names, ["ProviderKey/p", "Model/a", "Model/b", "APIKey/k"]);
    }
}
