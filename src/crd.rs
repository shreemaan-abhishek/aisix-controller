//! Custom resource types.
//!
//! A spec holds the AISIX resource document with its own (snake_case) field
//! names, plus a reference field that replaces the inline credential or id.
//! The schema is generated from aisix-core (see `schema.rs`), so the Rust
//! side keeps the spec as an untyped map.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const GROUP: &str = "aisix.api7.ai";
pub const VERSION: &str = "v1alpha1";

/// Pod-template annotation carrying the hash of the served resources file.
pub const HASH_ANNOTATION: &str = "aisix.api7.ai/resources-hash";

#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[kube(
    group = "aisix.api7.ai",
    version = "v1alpha1",
    kind = "ProviderKey",
    plural = "providerkeys",
    shortname = "aipk",
    namespaced,
    status = "ResourceStatus",
    schema = "disabled"
)]
pub struct ProviderKeySpec {
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[kube(
    group = "aisix.api7.ai",
    version = "v1alpha1",
    kind = "Model",
    plural = "models",
    shortname = "aimodel",
    namespaced,
    status = "ResourceStatus",
    schema = "disabled"
)]
pub struct ModelSpec {
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[kube(
    group = "aisix.api7.ai",
    version = "v1alpha1",
    kind = "APIKey",
    plural = "apikeys",
    shortname = "aikey",
    namespaced,
    status = "ResourceStatus",
    schema = "disabled"
)]
pub struct APIKeySpec {
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// The three resource kinds v0.1 reconciles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    ProviderKey,
    Model,
    APIKey,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::ProviderKey, Kind::Model, Kind::APIKey];

    /// Kubernetes kind name.
    pub fn name(self) -> &'static str {
        match self {
            Kind::ProviderKey => "ProviderKey",
            Kind::Model => "Model",
            Kind::APIKey => "APIKey",
        }
    }

    /// Collection key in the resources file.
    pub fn collection(self) -> &'static str {
        match self {
            Kind::ProviderKey => "provider_keys",
            Kind::Model => "models",
            Kind::APIKey => "api_keys",
        }
    }

    /// Resource name in aisix-core's schema registry.
    pub fn schema_resource(self) -> &'static str {
        match self {
            Kind::ProviderKey => "provider_key",
            Kind::Model => "model",
            Kind::APIKey => "api_key",
        }
    }

    pub fn from_collection(collection: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.collection() == collection)
    }
}

/// Identity field shared by all three kinds; defaults to `metadata.name`.
pub const IDENTITY_FIELD: &str = "display_name";

/// ProviderKey: Secret holding the upstream credential.
pub const API_KEY_SECRET_REF: &str = "api_key_secret_ref";
/// Model: the ProviderKey this model authenticates with.
pub const PROVIDER_KEY_REF: &str = "provider_key_ref";
/// APIKey: Secret holding the caller's plaintext key.
pub const KEY_SECRET_REF: &str = "key_secret_ref";
