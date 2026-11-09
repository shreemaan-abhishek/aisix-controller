//! Rendered entries -> one validated resources file.
//!
//! The document is checked with the gateway's own pipeline
//! (`aisix_core::filesource::load_from_str`) before it is written. An entry
//! the gateway would reject is replaced by the version currently served, so
//! one bad resource never takes the others down. With no served version it is
//! left out.

use std::collections::BTreeMap;

use aisix_core::filesource::load_from_str;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::crd::{Kind, IDENTITY_FIELD};
use crate::render::{Entry, Source, SourceKey};

pub const FORMAT_VERSION: &str = "1";

/// What happened to one custom resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Its current spec is in the document.
    Applied,
    /// Rejected; the previously served version stays in the document.
    Retained(String),
    /// Rejected, and not served at all.
    Rejected(String),
}

#[derive(Debug)]
pub struct Outcome {
    /// The document to serve, or `None` when it could not be made valid.
    pub document: Option<String>,
    pub dispositions: BTreeMap<SourceKey, Disposition>,
    /// Set when the document as a whole failed.
    pub error: Option<String>,
}

/// Entries of the currently served document, by collection and identity.
pub type Served = BTreeMap<(Kind, String), Entry>;

/// Parse the served document. Anything unreadable counts as nothing served.
pub fn served_entries(document: Option<&str>) -> Served {
    let mut out = Served::new();
    let Some(Ok(Value::Object(root))) = document.map(serde_json::from_str::<Value>) else {
        return out;
    };
    for kind in Kind::ALL {
        let Some(Value::Array(items)) = root.get(kind.collection()) else {
            continue;
        };
        for item in items {
            if let Some(id) = item.get(IDENTITY_FIELD).and_then(Value::as_str) {
                out.insert((kind, id.replace("$$", "$")), item.clone());
            }
        }
    }
    out
}

pub fn document(entries: &[(Kind, Entry)]) -> String {
    let mut root = Map::new();
    root.insert(
        "_format_version".into(),
        Value::String(FORMAT_VERSION.into()),
    );
    for kind in Kind::ALL {
        let items: Vec<Value> = entries
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, e)| e.clone())
            .collect();
        if !items.is_empty() {
            root.insert(kind.collection().into(), Value::Array(items));
        }
    }
    let mut out = serde_json::to_string_pretty(&Value::Object(root)).expect("document serializes");
    out.push('\n');
    out
}

pub fn hash(document: &str) -> String {
    hex::encode(Sha256::digest(document.as_bytes()))
}

/// Run the gateway's resources-file pipeline over a document.
/// Errors come back as `(scope, message)`.
pub fn check(document: &str) -> Result<(), Vec<(String, String)>> {
    load_from_str(document, "aisix-controller", 1, &|_| None)
        .map(|_| ())
        .map_err(|e| e.errors.into_iter().map(|e| (e.scope, e.message)).collect())
}

/// `models[2] ("gpt")` -> (Model, 2).
fn parse_scope(scope: &str) -> Option<(Kind, usize)> {
    let (collection, rest) = scope.split_once('[')?;
    let (index, _) = rest.split_once(']')?;
    Some((Kind::from_collection(collection)?, index.parse().ok()?))
}

#[derive(Clone, Debug)]
enum State {
    Current(Entry),
    Served(Entry, String),
    Dropped(String),
}

pub fn assemble(rendered: Vec<(Source, Result<Entry, String>)>, served: &Served) -> Outcome {
    let fallback = |s: &Source, reason: String| match served.get(&(s.kind, s.identity())) {
        Some(e) => State::Served(e.clone(), reason),
        None => State::Dropped(reason),
    };

    let mut items: Vec<(Source, State)> = rendered
        .into_iter()
        .map(|(s, r)| {
            let state = match r {
                Ok(e) => State::Current(e),
                Err(reason) => fallback(&s, reason),
            };
            (s, state)
        })
        .collect();

    // Each round drops at least one entry, so this terminates.
    let rounds = items.len() * 2 + 1;
    for _ in 0..rounds {
        let mut included: Vec<(Kind, Entry)> = Vec::new();
        let mut positions: BTreeMap<(Kind, usize), usize> = BTreeMap::new();
        let mut per_kind: BTreeMap<Kind, usize> = BTreeMap::new();
        for (i, (s, st)) in items.iter().enumerate() {
            let entry = match st {
                State::Current(e) | State::Served(e, _) => e,
                State::Dropped(_) => continue,
            };
            let n = per_kind.entry(s.kind).or_default();
            positions.insert((s.kind, *n), i);
            *n += 1;
            included.push((s.kind, entry.clone()));
        }
        let doc = document(&included);
        let errors = match check(&doc) {
            Ok(()) => return finish(items, Some(doc), None),
            Err(errors) => errors,
        };

        let mut faults: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (scope, message) in &errors {
            match parse_scope(scope).and_then(|p| positions.get(&p)) {
                Some(i) => faults.entry(*i).or_default().push(message.clone()),
                None => {
                    let all = errors
                        .iter()
                        .map(|(s, m)| format!("{s}: {m}"))
                        .collect::<Vec<_>>()
                        .join("; ");
                    return finish(items, None, Some(all));
                }
            }
        }
        for (i, messages) in faults {
            let reason = messages.join("; ");
            let (s, st) = &mut items[i];
            *st = match st {
                State::Current(_) => fallback(s, reason),
                State::Served(_, earlier) => State::Dropped(format!(
                    "{earlier}; the previously served version is no longer valid either: {reason}"
                )),
                State::Dropped(r) => State::Dropped(r.clone()),
            };
        }
    }
    finish(
        items,
        None,
        Some("the resources file did not converge to a valid state".into()),
    )
}

fn finish(items: Vec<(Source, State)>, document: Option<String>, error: Option<String>) -> Outcome {
    let dispositions = items
        .into_iter()
        .map(|(s, st)| {
            let d = match st {
                State::Current(_) => Disposition::Applied,
                State::Served(_, r) => Disposition::Retained(r),
                State::Dropped(r) => Disposition::Rejected(r),
            };
            (s.key(), d)
        })
        .collect();
    Outcome {
        document,
        dispositions,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{render_all, Secrets};
    use serde_json::json;

    fn source(kind: Kind, name: &str, fields: Value) -> Source {
        Source {
            kind,
            name: name.into(),
            generation: Some(1),
            created: None,
            fields: fields.as_object().unwrap().clone(),
        }
    }

    fn secrets() -> Secrets {
        let mut s = Secrets::new();
        s.insert(
            "creds".into(),
            BTreeMap::from([
                ("pk".into(), b"sk-test".to_vec()),
                ("caller".into(), b"caller".to_vec()),
            ]),
        );
        s
    }

    fn provider() -> Source {
        source(
            Kind::ProviderKey,
            "mock",
            json!({"provider": "openai", "adapter": "openai", "api_base": "http://mock:8080/v1", "api_key_secret_ref": {"name": "creds", "key": "pk"}}),
        )
    }

    fn model(name: &str, provider_ref: &str) -> Source {
        source(
            Kind::Model,
            name,
            json!({"provider": "openai", "model_name": "upstream", "provider_key_ref": {"name": provider_ref}}),
        )
    }

    fn api_key() -> Source {
        source(
            Kind::APIKey,
            "caller",
            json!({"allowed_models": ["*"], "key_secret_ref": {"name": "creds", "key": "caller"}}),
        )
    }

    fn run(sources: &[Source], served: &Served) -> Outcome {
        assemble(render_all(sources, &secrets()), served)
    }

    #[test]
    fn empty_document_is_valid() {
        assert_eq!(check(&document(&[])), Ok(()));
    }

    #[test]
    fn valid_resources_produce_a_loadable_document() {
        let out = run(
            &[provider(), model("chat", "mock"), api_key()],
            &Served::new(),
        );
        let doc = out.document.expect("document");
        assert!(out.error.is_none());
        assert!(out
            .dispositions
            .values()
            .all(|d| *d == Disposition::Applied));
        assert_eq!(check(&doc), Ok(()));
        let v: Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(v["_format_version"], json!("1"));
        assert_eq!(v["models"][0]["provider_key"], json!("mock"));
    }

    #[test]
    fn gateway_rejection_is_attributed_to_the_resource() {
        let bad = source(
            Kind::Model,
            "bad",
            json!({"provider": "openai", "model_name": "x", "provider_key_ref": {"name": "mock"}, "timeout": "soon"}),
        );
        let out = run(&[provider(), model("good", "mock"), bad], &Served::new());
        assert!(out.document.is_some());
        let key = SourceKey {
            kind: Kind::Model,
            name: "bad".into(),
        };
        match &out.dispositions[&key] {
            Disposition::Rejected(m) => assert!(m.contains("timeout"), "{m}"),
            d => panic!("{d:?}"),
        }
        let good = SourceKey {
            kind: Kind::Model,
            name: "good".into(),
        };
        assert_eq!(out.dispositions[&good], Disposition::Applied);
    }

    #[test]
    fn rejected_update_keeps_the_served_version() {
        let first = run(&[provider(), model("chat", "mock")], &Served::new());
        let served = served_entries(first.document.as_deref());

        let broken = source(
            Kind::Model,
            "chat",
            json!({"provider": "openai", "model_name": "upstream", "provider_key_ref": {"name": "mock"}, "timeout": -1}),
        );
        let out = run(&[provider(), broken], &served);
        let key = SourceKey {
            kind: Kind::Model,
            name: "chat".into(),
        };
        match &out.dispositions[&key] {
            Disposition::Retained(m) => assert!(m.contains("timeout"), "{m}"),
            d => panic!("{d:?}"),
        }
        assert_eq!(out.document, first.document);
    }

    #[test]
    fn dangling_reference_cascades_to_dependents() {
        let broken_provider = source(
            Kind::ProviderKey,
            "mock",
            json!({"api_key_secret_ref": {"name": "gone", "key": "pk"}}),
        );
        let out = run(&[broken_provider, model("chat", "mock")], &Served::new());
        let pk = SourceKey {
            kind: Kind::ProviderKey,
            name: "mock".into(),
        };
        let chat = SourceKey {
            kind: Kind::Model,
            name: "chat".into(),
        };
        assert!(matches!(out.dispositions[&pk], Disposition::Rejected(_)));
        match &out.dispositions[&chat] {
            Disposition::Rejected(m) => assert!(m.contains("mock"), "{m}"),
            d => panic!("{d:?}"),
        }
        assert_eq!(check(out.document.as_deref().unwrap()), Ok(()));
    }

    #[test]
    fn cross_field_rules_dropped_from_the_crd_are_still_enforced() {
        let first = run(&[provider(), model("chat", "mock")], &Served::new());
        let served = served_entries(first.document.as_deref());

        // A direct model that also routes: the CRD has no oneOf, the gateway does.
        let both = source(
            Kind::Model,
            "chat",
            json!({"provider": "openai", "model_name": "upstream", "provider_key_ref": {"name": "mock"},
                   "routing": {"targets": [{"model": "chat"}]}}),
        );
        let out = run(&[provider(), both], &served);
        let key = SourceKey {
            kind: Kind::Model,
            name: "chat".into(),
        };
        assert!(
            matches!(out.dispositions[&key], Disposition::Retained(_)),
            "{:?}",
            out.dispositions
        );
        assert_eq!(out.document, first.document);
    }

    #[test]
    fn deleted_resources_leave_the_document() {
        let first = run(&[provider(), model("chat", "mock")], &Served::new());
        let served = served_entries(first.document.as_deref());
        let out = run(&[provider()], &served);
        let v: Value = serde_json::from_str(out.document.as_deref().unwrap()).unwrap();
        assert!(v.get("models").is_none());
    }

    #[test]
    fn scopes_parse() {
        assert_eq!(parse_scope("models[2] (\"gpt\")"), Some((Kind::Model, 2)));
        assert_eq!(parse_scope("api_keys[0]"), Some((Kind::APIKey, 0)));
        assert_eq!(parse_scope("(file)"), None);
    }

    #[test]
    fn served_identities_are_unescaped() {
        let doc = document(&[(Kind::Model, json!({"display_name": "a$$b"}))]);
        let served = served_entries(Some(&doc));
        assert!(served.contains_key(&(Kind::Model, "a$b".into())));
    }
}
