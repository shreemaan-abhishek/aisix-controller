//! CRD generation from aisix-core's strict resource schemas.
//!
//! aisix-core publishes draft-07 JSON Schemas (`resource_root_schema`). A
//! CRD needs a structural OpenAPI v3 schema, so this module:
//!
//! 1. shapes each resource for Kubernetes: drops the inline credential and
//!    id-form references, and adds the reference fields the controller
//!    resolves (`crd.rs`);
//! 2. converts the result to a structural schema: `$ref`s inlined, nullable
//!    types folded, composition keywords merged or dropped.
//!
//! The conversion keeps types, required fields, enums, ranges, lengths and
//! patterns. It drops what a structural schema cannot carry (exclusive
//! `oneOf` branches, `if`/`then`, `not`, defaults, formats). Those rules are
//! still enforced: the controller validates every document with the
//! gateway's own pipeline before writing it.

use std::collections::BTreeSet;

use aisix_core::filesource::{mcp_ref_id_fields, model_ref_id_fields};
use aisix_core::models::schema::resource_root_schema;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::{
    CustomResourceColumnDefinition, CustomResourceDefinition, CustomResourceValidation,
    JSONSchemaProps,
};
use kube::CustomResourceExt;
use serde_json::{json, Map, Value};

use crate::crd::{
    APIKey, Kind, Model, ProviderKey, API_KEY_SECRET_REF, IDENTITY_FIELD, KEY_SECRET_REF,
    PROVIDER_KEY_REF,
};

/// Fields removed from a kind's spec, at any depth.
fn removed_fields(kind: Kind) -> BTreeSet<&'static str> {
    let mut out: BTreeSet<&'static str> = BTreeSet::new();
    match kind {
        Kind::ProviderKey => {
            out.extend(["api_key", "secret"]);
        }
        Kind::Model => {
            out.insert("provider_key_id");
        }
        Kind::APIKey => {
            out.insert("key_hash");
        }
    }
    // The resources file refuses id-form references; they are written by name.
    for f in model_ref_id_fields(kind.collection()) {
        out.insert(f.field);
    }
    for f in mcp_ref_id_fields(kind.collection()) {
        out.insert(f.field);
    }
    out
}

fn secret_key_ref(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "required": ["name", "key"],
        "properties": {
            "name": {"type": "string", "minLength": 1, "description": "Secret name, in the resource's namespace."},
            "key": {"type": "string", "minLength": 1, "description": "Key within the Secret."}
        }
    })
}

/// Reference fields the controller resolves, with whether each is required.
fn added_fields(kind: Kind) -> Vec<(&'static str, Value, bool)> {
    match kind {
        Kind::ProviderKey => vec![(
            API_KEY_SECRET_REF,
            secret_key_ref("Secret key holding the upstream provider credential. The controller writes it as `api_key`."),
            true,
        )],
        Kind::Model => vec![(
            PROVIDER_KEY_REF,
            json!({
                "type": "object",
                "description": "ProviderKey, in the same namespace, this model authenticates with. Direct models need it; routing, ensemble and semantic models leave it unset.",
                "required": ["name"],
                "properties": {
                    "name": {"type": "string", "minLength": 1, "description": "ProviderKey `metadata.name`."}
                }
            }),
            false,
        )],
        Kind::APIKey => vec![(
            KEY_SECRET_REF,
            secret_key_ref("Secret key holding the caller's plaintext API key. The controller writes only its SHA-256 hash."),
            true,
        )],
    }
}

/// The aisix-core strict schema for a kind, shaped for Kubernetes.
/// Still draft-07 (with `definitions`); see [`to_structural`].
pub fn shaped_schema(kind: Kind) -> Value {
    let mut root = resource_root_schema(kind.schema_resource(), true);
    let removed = removed_fields(kind);
    strip_fields(&mut root, &removed);

    let obj = root.as_object_mut().expect("resource schema is an object");
    // Root compositions only encode cross-field rules (credential spelling,
    // model shape), which the gateway pipeline enforces.
    for k in ["anyOf", "oneOf", "allOf", "if", "then", "else", "not"] {
        obj.remove(k);
    }

    let mut required: Vec<Value> = obj
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|v| v.as_str() != Some(IDENTITY_FIELD))
        .collect();

    let props = obj
        .entry("properties")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("properties is an object");
    if let Some(Value::Object(identity)) = props.get_mut(IDENTITY_FIELD) {
        let base = identity
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        identity.insert(
            "description".into(),
            Value::String(
                format!("{base} Defaults to `metadata.name`.",)
                    .trim()
                    .to_string(),
            ),
        );
    }
    for (name, schema, is_required) in added_fields(kind) {
        props.insert(name.to_string(), schema);
        if is_required {
            required.push(Value::String(name.to_string()));
        }
    }
    if required.is_empty() {
        obj.remove("required");
    } else {
        obj.insert("required".into(), Value::Array(required));
    }
    root
}

/// Remove the named properties everywhere: in `properties`, `required`, and
/// inside definitions and compositions.
fn strip_fields(node: &mut Value, removed: &BTreeSet<&str>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::Object(props)) = map.get_mut("properties") {
                props.retain(|k, _| !removed.contains(k.as_str()));
            }
            if let Some(Value::Array(req)) = map.get_mut("required") {
                req.retain(|v| v.as_str().is_none_or(|s| !removed.contains(s)));
            }
            for (_, v) in map.iter_mut() {
                strip_fields(v, removed);
            }
        }
        Value::Array(items) => {
            for v in items {
                strip_fields(v, removed);
            }
        }
        _ => {}
    }
}

const PRESERVE: &str = "x-kubernetes-preserve-unknown-fields";
const INT_OR_STRING: &str = "x-kubernetes-int-or-string";

/// Keywords copied as-is onto a structural node.
const KEPT: &[&str] = &[
    "description",
    "enum",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
    "required",
];

fn preserve(description: Option<&Value>) -> Value {
    let mut m = Map::new();
    if let Some(d) = description {
        m.insert("description".into(), d.clone());
    }
    m.insert(PRESERVE.into(), Value::Bool(true));
    Value::Object(m)
}

/// Convert a draft-07 schema (with its root `definitions`) to a structural
/// OpenAPI v3 schema.
pub fn to_structural(root: &Value) -> Value {
    let defs = root
        .get("definitions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut stack = Vec::new();
    let mut out = convert(root, &defs, &mut stack);
    if let Value::Object(m) = &mut out {
        // A spec is always an object.
        m.remove(PRESERVE);
        m.insert("type".into(), json!("object"));
    }
    out
}

/// True when a composition branch only constrains values (required fields,
/// negations) and adds no shape of its own.
fn is_constraint_only(branch: &Value) -> bool {
    match branch {
        Value::Object(m) => m.keys().all(|k| {
            matches!(
                k.as_str(),
                "required" | "not" | "title" | "description" | "if" | "then" | "else"
            )
        }),
        _ => false,
    }
}

fn is_null_branch(branch: &Value) -> bool {
    branch.get("type").and_then(Value::as_str) == Some("null")
}

/// Resolve `$ref` and single-branch `allOf` into one flat schema object,
/// keeping the referring node's own keywords (its description wins).
fn flatten(
    node: &Map<String, Value>,
    defs: &Map<String, Value>,
    stack: &mut Vec<String>,
) -> Option<Map<String, Value>> {
    let mut out = node.clone();
    if let Some(r) = out.remove("$ref") {
        let name = r.as_str()?.strip_prefix("#/definitions/")?.to_string();
        if stack.contains(&name) {
            return None;
        }
        let target = defs.get(&name)?.as_object()?.clone();
        stack.push(name);
        let resolved = flatten(&target, defs, stack);
        stack.pop();
        let mut merged = resolved?;
        for (k, v) in out {
            merged.insert(k, v);
        }
        out = merged;
    }
    if let Some(Value::Array(all)) = out.get("allOf").cloned() {
        let shaped: Vec<&Value> = all.iter().filter(|b| !is_constraint_only(b)).collect();
        if shaped.len() == 1 {
            out.remove("allOf");
            let branch = shaped[0].as_object()?;
            let mut merged = flatten(branch, defs, stack)?;
            for (k, v) in out {
                merged.insert(k, v);
            }
            out = merged;
        }
    }
    Some(out)
}

fn convert(node: &Value, defs: &Map<String, Value>, stack: &mut Vec<String>) -> Value {
    let map = match node {
        Value::Bool(true) => return preserve(None),
        Value::Object(m) if m.is_empty() => return preserve(None),
        Value::Object(m) => m,
        _ => return preserve(None),
    };
    let Some(mut m) = flatten(map, defs, stack) else {
        return preserve(map.get("description"));
    };

    let mut nullable = false;

    // anyOf / oneOf / multi-branch allOf.
    for key in ["anyOf", "oneOf", "allOf"] {
        let Some(Value::Array(branches)) = m.remove(key) else {
            continue;
        };
        let mut shaped = Vec::new();
        for b in branches {
            if is_null_branch(&b) {
                nullable = true;
            } else if !is_constraint_only(&b) {
                shaped.push(b);
            }
        }
        if shaped.is_empty() {
            continue;
        }
        let converted: Vec<Value> = shaped.iter().map(|b| convert(b, defs, stack)).collect();
        match merge_branches(&converted, key == "allOf") {
            Some(merged) => {
                for (k, v) in merged {
                    if k == "description" && m.contains_key("description") {
                        continue;
                    }
                    if k == "properties" {
                        let props = m.entry("properties").or_insert_with(|| json!({}));
                        if let (Value::Object(dst), Value::Object(src)) = (props, v) {
                            for (pk, pv) in src {
                                dst.entry(pk).or_insert(pv);
                            }
                        }
                        continue;
                    }
                    m.entry(k).or_insert(v);
                }
            }
            None => {
                let mut p = preserve(m.get("description")).as_object().cloned().unwrap();
                if converted.len() == 2
                    && converted
                        .iter()
                        .any(|c| c.get("type") == Some(&json!("integer")))
                    && converted
                        .iter()
                        .any(|c| c.get("type") == Some(&json!("string")))
                {
                    p.remove(PRESERVE);
                    p.insert(INT_OR_STRING.into(), Value::Bool(true));
                }
                if nullable {
                    p.insert("nullable".into(), Value::Bool(true));
                }
                return Value::Object(p);
            }
        }
    }

    // type: [X, "null"]
    let mut ty: Option<String> = None;
    match m.remove("type") {
        Some(Value::String(t)) if t == "null" => nullable = true,
        Some(Value::String(t)) => ty = Some(t),
        Some(Value::Array(ts)) => {
            let mut rest: Vec<String> = Vec::new();
            for t in ts {
                match t.as_str() {
                    Some("null") => nullable = true,
                    Some(s) => rest.push(s.to_string()),
                    None => {}
                }
            }
            match rest.len() {
                0 => {}
                1 => ty = rest.pop(),
                _ => {
                    let mut p = preserve(m.get("description")).as_object().cloned().unwrap();
                    if rest.len() == 2
                        && rest.contains(&"integer".to_string())
                        && rest.contains(&"string".to_string())
                    {
                        p.remove(PRESERVE);
                        p.insert(INT_OR_STRING.into(), Value::Bool(true));
                    }
                    if nullable {
                        p.insert("nullable".into(), Value::Bool(true));
                    }
                    return Value::Object(p);
                }
            }
        }
        _ => {}
    }

    let mut out = Map::new();
    for k in KEPT {
        if let Some(v) = m.get(*k) {
            out.insert((*k).to_string(), v.clone());
        }
    }
    if let Some(c) = m.get("const") {
        out.insert("enum".into(), json!([c]));
    }
    // draft-07 exclusive bounds are numbers; OpenAPI v3.0 uses booleans.
    for (ex, base) in [
        ("exclusiveMinimum", "minimum"),
        ("exclusiveMaximum", "maximum"),
    ] {
        if let Some(n) = m.get(ex).filter(|v| v.is_number()) {
            out.insert(base.into(), n.clone());
            out.insert(ex.into(), Value::Bool(true));
        }
    }
    if let Some(Value::Array(values)) = out.get_mut("enum") {
        if values.iter().any(Value::is_null) {
            nullable = true;
            values.retain(|v| !v.is_null());
        }
        if values.is_empty() {
            out.remove("enum");
        }
    }

    let props = m.get("properties").and_then(Value::as_object);
    let additional = m.get("additionalProperties");
    let items = m.get("items");

    if ty.is_none() {
        ty = if props.is_some() || additional.is_some() {
            Some("object".into())
        } else if items.is_some() {
            Some("array".into())
        } else if let Some(Value::Array(values)) = out.get("enum") {
            infer_enum_type(values)
        } else {
            None
        };
    }

    match ty.as_deref() {
        Some("object") => {
            let additional_schema = match additional {
                Some(Value::Object(a)) if !a.is_empty() => {
                    Some(convert(additional.unwrap(), defs, stack))
                }
                Some(Value::Bool(true)) | Some(Value::Object(_)) => Some(preserve(None)),
                _ => None,
            };
            let props_nonempty = props.is_some_and(|p| !p.is_empty());
            if let (Some(a), false) = (&additional_schema, props_nonempty) {
                out.insert("additionalProperties".into(), a.clone());
            } else if props_nonempty {
                let mut converted = Map::new();
                for (k, v) in props.unwrap() {
                    if k.is_empty() {
                        continue;
                    }
                    converted.insert(k.clone(), convert(v, defs, stack));
                }
                out.insert("properties".into(), Value::Object(converted));
            } else if let Some(a) = additional_schema {
                out.insert("additionalProperties".into(), a);
            } else {
                out.insert(PRESERVE.into(), Value::Bool(true));
            }
            out.insert("type".into(), json!("object"));
            prune_required(&mut out);
        }
        Some("array") => {
            let item = match items {
                Some(Value::Array(_)) | None => preserve(None),
                Some(i) => convert(i, defs, stack),
            };
            out.insert("items".into(), item);
            out.insert("type".into(), json!("array"));
            out.remove("required");
        }
        Some(t @ ("string" | "integer" | "number" | "boolean")) => {
            out.insert("type".into(), json!(t));
            out.remove("required");
            if let Some(Value::Array(values)) = out.get("enum") {
                if !values.iter().all(|v| matches_type(v, t)) {
                    out.remove("enum");
                }
            }
        }
        _ => {
            let mut p = preserve(m.get("description")).as_object().cloned().unwrap();
            if nullable {
                p.insert("nullable".into(), Value::Bool(true));
            }
            return Value::Object(p);
        }
    }
    if nullable {
        out.insert("nullable".into(), Value::Bool(true));
    }
    Value::Object(out)
}

/// Keep only `required` entries that name a declared property.
fn prune_required(out: &mut Map<String, Value>) {
    let declared: BTreeSet<String> = out
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(Value::Array(req)) = out.get_mut("required") {
        req.retain(|v| v.as_str().is_some_and(|s| declared.contains(s)));
        if req.is_empty() {
            out.remove("required");
        }
    }
}

fn matches_type(v: &Value, t: &str) -> bool {
    match t {
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        _ => false,
    }
}

fn infer_enum_type(values: &[Value]) -> Option<String> {
    for t in ["string", "integer", "number", "boolean"] {
        if !values.is_empty() && values.iter().all(|v| matches_type(v, t)) {
            return Some(t.into());
        }
    }
    None
}

/// Merge converted branches into one shape, or `None` when their types differ.
/// `all` (allOf) keeps the union of `required`; anyOf/oneOf drop it.
fn merge_branches(branches: &[Value], all: bool) -> Option<Map<String, Value>> {
    let types: BTreeSet<Option<&str>> = branches
        .iter()
        .map(|b| b.get("type").and_then(Value::as_str))
        .collect();
    if types.len() != 1 || types.contains(&None) {
        return None;
    }
    let ty = types.into_iter().next().flatten()?;
    let mut out = Map::new();
    out.insert("type".into(), json!(ty));
    match ty {
        "object" => {
            let mut props = Map::new();
            let mut required: BTreeSet<String> = BTreeSet::new();
            let mut any_preserve = false;
            for b in branches {
                if let Some(Value::Object(p)) = b.get("properties") {
                    for (k, v) in p {
                        props.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                if b.get(PRESERVE) == Some(&Value::Bool(true))
                    || b.get("additionalProperties").is_some()
                {
                    any_preserve = true;
                }
                if all {
                    if let Some(Value::Array(r)) = b.get("required") {
                        required.extend(r.iter().filter_map(|v| v.as_str().map(String::from)));
                    }
                }
            }
            if any_preserve {
                // Branch shapes disagree on openness; keep everything.
                out.insert(PRESERVE.into(), Value::Bool(true));
            }
            if !props.is_empty() {
                out.insert("properties".into(), Value::Object(props));
            }
            if !required.is_empty() {
                out.insert(
                    "required".into(),
                    Value::Array(required.into_iter().map(Value::String).collect()),
                );
            }
        }
        "array" => {
            out.insert("items".into(), preserve(None));
        }
        _ => {
            let mut values: Vec<Value> = Vec::new();
            let mut all_enum = true;
            for b in branches {
                match b.get("enum") {
                    Some(Value::Array(e)) => {
                        for v in e {
                            if !values.contains(v) {
                                values.push(v.clone());
                            }
                        }
                    }
                    _ => all_enum = false,
                }
            }
            if all_enum && !values.is_empty() {
                out.insert("enum".into(), Value::Array(values));
            }
        }
    }
    Some(out)
}

fn status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "observedGeneration": {"type": "integer", "format": "int64"},
            "conditions": {
                "type": "array",
                "x-kubernetes-list-type": "map",
                "x-kubernetes-list-map-keys": ["type"],
                "items": {
                    "type": "object",
                    "required": ["type", "status", "lastTransitionTime", "reason", "message"],
                    "properties": {
                        "type": {"type": "string"},
                        "status": {"type": "string", "enum": ["True", "False", "Unknown"]},
                        "observedGeneration": {"type": "integer", "format": "int64"},
                        "lastTransitionTime": {"type": "string", "format": "date-time"},
                        "reason": {"type": "string"},
                        "message": {"type": "string"}
                    }
                }
            }
        }
    })
}

/// The full structural schema for a kind's custom resource.
pub fn resource_schema(kind: Kind) -> Value {
    let spec = to_structural(&shaped_schema(kind));
    json!({
        "type": "object",
        "description": format!("AISIX {} resource.", kind.name()),
        "required": ["spec"],
        "properties": {
            "apiVersion": {"type": "string"},
            "kind": {"type": "string"},
            "metadata": {"type": "object"},
            "spec": spec,
            "status": status_schema()
        }
    })
}

fn condition_column(name: &str) -> CustomResourceColumnDefinition {
    CustomResourceColumnDefinition {
        name: name.to_string(),
        type_: "string".to_string(),
        json_path: format!(".status.conditions[?(@.type==\"{name}\")].status"),
        ..Default::default()
    }
}

/// The CustomResourceDefinition for a kind.
pub fn crd(kind: Kind) -> CustomResourceDefinition {
    let mut crd = match kind {
        Kind::ProviderKey => ProviderKey::crd(),
        Kind::Model => Model::crd(),
        Kind::APIKey => APIKey::crd(),
    };
    let props: JSONSchemaProps =
        serde_json::from_value(resource_schema(kind)).expect("structural schema deserializes");
    for v in &mut crd.spec.versions {
        v.schema = Some(CustomResourceValidation {
            open_api_v3_schema: Some(props.clone()),
        });
        v.additional_printer_columns = Some(vec![
            condition_column("Accepted"),
            condition_column("Programmed"),
            CustomResourceColumnDefinition {
                name: "Age".into(),
                type_: "date".into(),
                json_path: ".metadata.creationTimestamp".into(),
                ..Default::default()
            },
        ]);
    }
    crd
}

/// CRD manifest as YAML, with a generated-file header.
pub fn crd_yaml(kind: Kind) -> String {
    let body = serde_yaml::to_string(&crd(kind)).expect("CRD serializes");
    format!("# Generated by `aisix-controller crds` from aisix-core. Do not edit.\n{body}")
}

/// File name a kind's CRD is written to.
pub fn crd_file_name(kind: Kind) -> String {
    let crd = crd(kind);
    format!("{}.yaml", crd.metadata.name.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk<'a>(node: &'a Value, path: String, f: &mut dyn FnMut(&str, &'a Map<String, Value>)) {
        if let Value::Object(m) = node {
            f(&path, m);
            if let Some(Value::Object(p)) = m.get("properties") {
                for (k, v) in p {
                    walk(v, format!("{path}.{k}"), f);
                }
            }
            if let Some(i) = m.get("items") {
                walk(i, format!("{path}[]"), f);
            }
            if let Some(a @ Value::Object(_)) = m.get("additionalProperties") {
                walk(a, format!("{path}{{}}"), f);
            }
        }
    }

    /// The subset of apiextensions' structural rules a generator can break.
    fn assert_structural(schema: &Value) {
        let forbidden = [
            "$ref",
            "$schema",
            "definitions",
            "oneOf",
            "anyOf",
            "allOf",
            "not",
            "if",
            "then",
            "else",
            "const",
            "default",
            "title",
            "propertyNames",
            "patternProperties",
            "uniqueItems",
            "examples",
        ];
        walk(schema, "spec".into(), &mut |path, m| {
            for k in forbidden {
                assert!(!m.contains_key(k), "{path}: forbidden keyword {k}");
            }
            let open = m.get(PRESERVE) == Some(&Value::Bool(true))
                || m.get(INT_OR_STRING) == Some(&Value::Bool(true));
            assert!(open || m.contains_key("type"), "{path}: node without type");
            if m.contains_key("properties") {
                assert!(
                    !m.contains_key("additionalProperties"),
                    "{path}: properties with additionalProperties"
                );
                assert_eq!(
                    m.get("type"),
                    Some(&json!("object")),
                    "{path}: properties on a non-object"
                );
            }
            if m.get("type") == Some(&json!("array")) {
                assert!(m.contains_key("items"), "{path}: array without items");
            }
            if let Some(Value::Bool(_)) = m.get("additionalProperties") {
                panic!("{path}: boolean additionalProperties");
            }
            if let Some(ex) = m.get("exclusiveMinimum") {
                assert!(ex.is_boolean(), "{path}: numeric exclusiveMinimum");
            }
        });
    }

    fn spec(kind: Kind) -> Value {
        resource_schema(kind)["properties"]["spec"].clone()
    }

    fn has_property_anywhere(node: &Value, name: &str) -> bool {
        let mut found = false;
        walk(node, String::new(), &mut |_, m| {
            if let Some(Value::Object(p)) = m.get("properties") {
                found |= p.contains_key(name);
            }
        });
        found
    }

    #[test]
    fn every_spec_is_structural() {
        for kind in Kind::ALL {
            assert_structural(&spec(kind));
        }
    }

    #[test]
    fn credentials_never_appear_inline() {
        let pk = spec(Kind::ProviderKey);
        assert!(!has_property_anywhere(&pk, "api_key"));
        assert!(!has_property_anywhere(&pk, "secret"));
        assert!(pk["properties"][API_KEY_SECRET_REF].is_object());
        assert!(pk["required"]
            .as_array()
            .unwrap()
            .contains(&json!(API_KEY_SECRET_REF)));

        let key = spec(Kind::APIKey);
        assert!(!has_property_anywhere(&key, "key_hash"));
        assert!(key["required"]
            .as_array()
            .unwrap()
            .contains(&json!(KEY_SECRET_REF)));
    }

    #[test]
    fn id_form_references_are_removed() {
        let model = spec(Kind::Model);
        for f in [
            "provider_key_id",
            "model_id",
            "target_id",
            "embedding_model_id",
            "default_id",
        ] {
            assert!(!has_property_anywhere(&model, f), "model still has {f}");
        }
        assert!(model["properties"][PROVIDER_KEY_REF].is_object());
        assert!(has_property_anywhere(&model, "routing"));

        let key = spec(Kind::APIKey);
        for f in [
            "allowed_model_ids",
            "mcp_rate_limits_by_id",
            "allow_ids",
            "deny_ids",
        ] {
            assert!(!has_property_anywhere(&key, f), "api key still has {f}");
        }
        assert!(has_property_anywhere(&key, "allowed_models"));
    }

    #[test]
    fn identity_is_optional() {
        for kind in Kind::ALL {
            let s = spec(kind);
            let required = s
                .get("required")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            assert!(!required.contains(&json!(IDENTITY_FIELD)), "{kind:?}");
            assert!(s["properties"][IDENTITY_FIELD].is_object(), "{kind:?}");
        }
    }

    #[test]
    fn nullable_and_refs_convert() {
        let root = json!({
            "definitions": {"Inner": {"type": "object", "properties": {"a": {"type": "integer", "minimum": 0.0}}}},
            "type": "object",
            "properties": {
                "opt": {"type": ["string", "null"]},
                "inner": {"anyOf": [{"$ref": "#/definitions/Inner"}, {"type": "null"}]},
                "wrapped": {"allOf": [{"$ref": "#/definitions/Inner"}], "description": "kept"},
                "mixed": {"anyOf": [{"type": "string"}, {"type": "object", "properties": {"x": {"type": "string"}}}]},
                "ios": {"type": ["integer", "string"]},
                "c": {"const": "fixed"},
                "map": {"type": "object", "additionalProperties": {"type": "string"}},
                "free": {"type": "object"},
                "ex": {"type": "number", "exclusiveMinimum": 0.0}
            }
        });
        let s = to_structural(&root);
        let p = &s["properties"];
        assert_eq!(p["opt"], json!({"type": "string", "nullable": true}));
        assert_eq!(p["inner"]["nullable"], json!(true));
        assert_eq!(p["inner"]["properties"]["a"]["type"], json!("integer"));
        assert_eq!(p["wrapped"]["description"], json!("kept"));
        assert_eq!(p["wrapped"]["properties"]["a"]["minimum"], json!(0.0));
        assert_eq!(p["mixed"][PRESERVE], json!(true));
        assert_eq!(p["ios"][INT_OR_STRING], json!(true));
        assert_eq!(p["c"], json!({"type": "string", "enum": ["fixed"]}));
        assert_eq!(p["map"]["additionalProperties"], json!({"type": "string"}));
        assert_eq!(p["free"][PRESERVE], json!(true));
        assert_eq!(p["ex"]["exclusiveMinimum"], json!(true));
        assert_structural(&s);
    }

    #[test]
    fn committed_crds_match_generator() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/crds");
        for kind in Kind::ALL {
            let path = dir.join(crd_file_name(kind));
            let committed = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}; run `make crds`", path.display()));
            assert!(
                committed == crd_yaml(kind),
                "{} is stale; run `make crds`",
                path.display()
            );
        }
    }
}
