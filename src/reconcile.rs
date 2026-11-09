//! The reconcile loop.
//!
//! The resources file is one document, so the controller reconciles the
//! whole namespace at once: any change to a custom resource, a Secret or the
//! gateway Deployment re-renders the document. A changed document is written
//! to the gateway's resources Secret and rolled out by stamping its hash on
//! the Deployment's pod template.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::{stream, StreamExt};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use k8s_openapi::ByteString;
use kube::api::{Api, ObjectMeta, Patch, PatchParams};
use kube::runtime::reflector::{self, Store};
use kube::runtime::{watcher, WatchStreamExt};
use kube::{Client, Resource, ResourceExt};
use serde_json::json;
use tokio::sync::Notify;

use crate::assemble::{assemble, document, hash, served_entries, Disposition, Outcome};
use crate::crd::{APIKey, Kind, Model, ProviderKey, ResourceStatus, HASH_ANNOTATION};
use crate::render::{render_all, secrets_from, Source, SourceKey};

pub const MANAGER: &str = "aisix-controller";

#[derive(Clone, Debug)]
pub struct Settings {
    pub namespace: String,
    pub secret_name: String,
    pub secret_key: String,
    pub deployment_name: String,
    pub resync: Duration,
}

struct Stores {
    provider_keys: Store<ProviderKey>,
    models: Store<Model>,
    api_keys: Store<APIKey>,
    secrets: Store<Secret>,
    deployments: Store<Deployment>,
}

fn source<K>(kind: Kind, obj: &K, fields: &serde_json::Map<String, serde_json::Value>) -> Source
where
    K: Resource,
{
    let meta = obj.meta();
    Source {
        kind,
        name: meta.name.clone().unwrap_or_default(),
        generation: meta.generation,
        created: meta.creation_timestamp.as_ref().map(|t| t.0.to_string()),
        fields: fields.clone(),
    }
}

impl Stores {
    fn sources(&self) -> Vec<Source> {
        let mut out = Vec::new();
        out.extend(
            self.provider_keys
                .state()
                .iter()
                .map(|o| source(Kind::ProviderKey, o.as_ref(), &o.spec.fields)),
        );
        out.extend(
            self.models
                .state()
                .iter()
                .map(|o| source(Kind::Model, o.as_ref(), &o.spec.fields)),
        );
        out.extend(
            self.api_keys
                .state()
                .iter()
                .map(|o| source(Kind::APIKey, o.as_ref(), &o.spec.fields)),
        );
        out
    }
}

/// Watch everything the document depends on and reconcile on every change,
/// plus a periodic resync.
pub async fn run(client: Client, settings: Settings) -> anyhow::Result<()> {
    let ns = settings.namespace.as_str();
    let notify = Arc::new(Notify::new());

    let (pk_store, pk_writer) = reflector::store();
    let (model_store, model_writer) = reflector::store();
    let (key_store, key_writer) = reflector::store();
    let (secret_store, secret_writer) = reflector::store();
    let (deploy_store, deploy_writer) = reflector::store();

    let cfg = watcher::Config::default;
    let deploy_cfg = cfg().fields(&format!("metadata.name={}", settings.deployment_name));

    let streams = vec![
        watcher(Api::<ProviderKey>::namespaced(client.clone(), ns), cfg())
            .default_backoff()
            .reflect(pk_writer)
            .map(|_| ())
            .boxed(),
        watcher(Api::<Model>::namespaced(client.clone(), ns), cfg())
            .default_backoff()
            .reflect(model_writer)
            .map(|_| ())
            .boxed(),
        watcher(Api::<APIKey>::namespaced(client.clone(), ns), cfg())
            .default_backoff()
            .reflect(key_writer)
            .map(|_| ())
            .boxed(),
        watcher(Api::<Secret>::namespaced(client.clone(), ns), cfg())
            .default_backoff()
            .reflect(secret_writer)
            .map(|_| ())
            .boxed(),
        watcher(
            Api::<Deployment>::namespaced(client.clone(), ns),
            deploy_cfg,
        )
        .default_backoff()
        .reflect(deploy_writer)
        .map(|_| ())
        .boxed(),
    ];
    let trigger = notify.clone();
    tokio::spawn(async move {
        let mut events = stream::select_all(streams);
        while events.next().await.is_some() {
            trigger.notify_one();
        }
    });

    let stores = Stores {
        provider_keys: pk_store,
        models: model_store,
        api_keys: key_store,
        secrets: secret_store,
        deployments: deploy_store,
    };
    stores.provider_keys.wait_until_ready().await?;
    stores.models.wait_until_ready().await?;
    stores.api_keys.wait_until_ready().await?;
    stores.secrets.wait_until_ready().await?;
    stores.deployments.wait_until_ready().await?;
    tracing::info!(namespace = ns, secret = %settings.secret_name, deployment = %settings.deployment_name, "caches synced");

    loop {
        // Coalesce bursts of events into one pass.
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Err(e) = reconcile(&client, &settings, &stores).await {
            tracing::warn!(error = %format!("{e:#}"), "reconcile failed");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(settings.resync) => {}
        }
    }
}

/// State of the gateway rollout for the document hash it should serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rollout {
    Ready,
    Progressing(String),
    Missing,
}

pub fn rollout(deployment: Option<&Deployment>, want: &str) -> Rollout {
    let Some(d) = deployment else {
        return Rollout::Missing;
    };
    let stamped = d
        .spec
        .as_ref()
        .and_then(|s| s.template.metadata.as_ref())
        .and_then(|m| m.annotations.as_ref())
        .and_then(|a| a.get(HASH_ANNOTATION));
    if stamped.map(String::as_str) != Some(want) {
        return Rollout::Progressing(
            "the gateway Deployment does not carry this configuration yet".into(),
        );
    }
    let replicas = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let Some(status) = d.status.as_ref() else {
        return Rollout::Progressing("the gateway Deployment reports no status yet".into());
    };
    let observed = status.observed_generation.unwrap_or(0) >= d.metadata.generation.unwrap_or(0);
    let updated = status.updated_replicas.unwrap_or(0);
    let available = status.available_replicas.unwrap_or(0);
    let total = status.replicas.unwrap_or(0);
    if observed && updated == replicas && available == replicas && total == replicas {
        Rollout::Ready
    } else {
        Rollout::Progressing(format!(
            "rolling out: {updated}/{replicas} updated, {available}/{replicas} available"
        ))
    }
}

async fn reconcile(client: &Client, settings: &Settings, stores: &Stores) -> anyhow::Result<()> {
    let ns = settings.namespace.as_str();
    let secrets: Vec<Secret> = stores
        .secrets
        .state()
        .iter()
        .map(|s| s.as_ref().clone())
        .collect();
    let current = secrets
        .iter()
        .find(|s| s.metadata.name.as_deref() == Some(settings.secret_name.as_str()))
        .and_then(|s| s.data.as_ref())
        .and_then(|d| d.get(&settings.secret_key))
        .and_then(|b| String::from_utf8(b.0.clone()).ok());

    let sources = stores.sources();
    let outcome = assemble(
        render_all(&sources, &secrets_from(&secrets)),
        &served_entries(current.as_deref()),
    );
    if let Some(e) = &outcome.error {
        tracing::warn!(error = %e, "resources file rejected; keeping the served version");
    }

    // A gateway needs a file to start, so seed an empty one.
    let desired = match (&outcome.document, &current) {
        (Some(doc), _) => Some(doc.clone()),
        (None, None) => Some(document(&[])),
        (None, Some(_)) => None,
    };
    let served = match desired {
        Some(doc) if current.as_deref() != Some(doc.as_str()) => {
            write_secret(client, settings, &doc).await?;
            tracing::info!(hash = %&hash(&doc)[..12], "resources file written");
            doc
        }
        Some(doc) => doc,
        None => current.clone().unwrap_or_default(),
    };
    let served_hash = hash(&served);

    let deployment = stores
        .deployments
        .state()
        .into_iter()
        .find(|d| d.metadata.name.as_deref() == Some(settings.deployment_name.as_str()));
    let mut state = rollout(deployment.as_deref(), &served_hash);
    if matches!(state, Rollout::Progressing(_))
        && deployment
            .as_deref()
            .is_some_and(|d| !stamped(d, &served_hash))
    {
        stamp_deployment(client, settings, &served_hash).await?;
        tracing::info!(hash = %&served_hash[..12], "gateway rollout started");
        state = Rollout::Progressing("rollout started".into());
    }

    let in_served = outcome.document.as_deref() == Some(served.as_str());
    for s in &sources {
        let previous = status_of(stores, s);
        let status = desired_status(s, &outcome, in_served, &state, previous.as_ref());
        if previous.as_ref() != Some(&status) {
            patch_status(client, ns, &s.key(), &status).await?;
        }
    }
    Ok(())
}

fn stamped(d: &Deployment, want: &str) -> bool {
    d.spec
        .as_ref()
        .and_then(|s| s.template.metadata.as_ref())
        .and_then(|m| m.annotations.as_ref())
        .and_then(|a| a.get(HASH_ANNOTATION))
        .is_some_and(|h| h == want)
}

async fn write_secret(client: &Client, settings: &Settings, doc: &str) -> anyhow::Result<()> {
    let api: Api<Secret> = Api::namespaced(client.clone(), &settings.namespace);
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(settings.secret_name.clone()),
            labels: Some(BTreeMap::from([(
                "app.kubernetes.io/managed-by".to_string(),
                MANAGER.to_string(),
            )])),
            annotations: Some(BTreeMap::from([(HASH_ANNOTATION.to_string(), hash(doc))])),
            ..Default::default()
        },
        data: Some(BTreeMap::from([(
            settings.secret_key.clone(),
            ByteString(doc.as_bytes().to_vec()),
        )])),
        type_: Some("Opaque".into()),
        ..Default::default()
    };
    let params = PatchParams::apply(MANAGER).force();
    api.patch(&settings.secret_name, &params, &Patch::Apply(&secret))
        .await
        .with_context(|| format!("writing Secret {}", settings.secret_name))?;
    Ok(())
}

async fn stamp_deployment(client: &Client, settings: &Settings, hash: &str) -> anyhow::Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), &settings.namespace);
    let patch =
        json!({"spec": {"template": {"metadata": {"annotations": {HASH_ANNOTATION: hash}}}}});
    api.patch(
        &settings.deployment_name,
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await
    .with_context(|| format!("stamping Deployment {}", settings.deployment_name))?;
    Ok(())
}

fn status_of(stores: &Stores, s: &Source) -> Option<ResourceStatus> {
    let find = |name: &str| -> Option<Option<ResourceStatus>> {
        match s.kind {
            Kind::ProviderKey => stores
                .provider_keys
                .state()
                .into_iter()
                .find(|o| o.name_any() == name)
                .map(|o| o.status.clone()),
            Kind::Model => stores
                .models
                .state()
                .into_iter()
                .find(|o| o.name_any() == name)
                .map(|o| o.status.clone()),
            Kind::APIKey => stores
                .api_keys
                .state()
                .into_iter()
                .find(|o| o.name_any() == name)
                .map(|o| o.status.clone()),
        }
    };
    find(&s.name).flatten()
}

fn condition(
    type_: &str,
    status: &str,
    reason: &str,
    message: String,
    generation: Option<i64>,
    previous: Option<&ResourceStatus>,
) -> Condition {
    let prior = previous.and_then(|p| p.conditions.iter().find(|c| c.type_ == type_));
    let last_transition_time = match prior {
        Some(c) if c.status == status => c.last_transition_time.clone(),
        _ => Time(k8s_openapi::jiff::Timestamp::now()),
    };
    Condition {
        type_: type_.into(),
        status: status.into(),
        reason: reason.into(),
        message,
        observed_generation: generation,
        last_transition_time,
    }
}

/// Conditions for one resource. `Accepted`: its current spec is valid.
/// `Programmed`: the gateway serves its current spec.
pub fn desired_status(
    s: &Source,
    outcome: &Outcome,
    in_served: bool,
    rollout: &Rollout,
    previous: Option<&ResourceStatus>,
) -> ResourceStatus {
    let g = s.generation;
    let disposition = outcome.dispositions.get(&s.key());
    let (accepted, programmed) = match (disposition, &outcome.error) {
        (Some(Disposition::Applied), None) => {
            let accepted = condition("Accepted", "True", "Accepted", "valid".into(), g, previous);
            let programmed = match (in_served, rollout) {
                (true, Rollout::Ready) => condition(
                    "Programmed",
                    "True",
                    "Programmed",
                    "served by the gateway".into(),
                    g,
                    previous,
                ),
                (_, Rollout::Missing) => condition(
                    "Programmed",
                    "False",
                    "GatewayNotFound",
                    "the gateway Deployment does not exist".into(),
                    g,
                    previous,
                ),
                (_, Rollout::Progressing(m)) => {
                    condition("Programmed", "False", "Pending", m.clone(), g, previous)
                }
                (false, Rollout::Ready) => condition(
                    "Programmed",
                    "False",
                    "Pending",
                    "waiting for the resources file to be written".into(),
                    g,
                    previous,
                ),
            };
            (accepted, programmed)
        }
        (Some(Disposition::Retained(m)), None) => (
            condition("Accepted", "False", "Invalid", m.clone(), g, previous),
            condition(
                "Programmed",
                "False",
                "Invalid",
                "the previously accepted version keeps serving".into(),
                g,
                previous,
            ),
        ),
        (Some(Disposition::Rejected(m)), None) => (
            condition("Accepted", "False", "Invalid", m.clone(), g, previous),
            condition(
                "Programmed",
                "False",
                "Invalid",
                "not served".into(),
                g,
                previous,
            ),
        ),
        (Some(Disposition::Rejected(m)), Some(_)) | (Some(Disposition::Retained(m)), Some(_)) => (
            condition("Accepted", "False", "Invalid", m.clone(), g, previous),
            condition(
                "Programmed",
                "False",
                "Invalid",
                "not served".into(),
                g,
                previous,
            ),
        ),
        (_, Some(e)) => (
            condition(
                "Accepted",
                "Unknown",
                "Pending",
                "the resources file as a whole is invalid".into(),
                g,
                previous,
            ),
            condition(
                "Programmed",
                "False",
                "ConfigurationRejected",
                e.clone(),
                g,
                previous,
            ),
        ),
        (None, None) => (
            condition(
                "Accepted",
                "Unknown",
                "Pending",
                "not reconciled yet".into(),
                g,
                previous,
            ),
            condition(
                "Programmed",
                "Unknown",
                "Pending",
                "not reconciled yet".into(),
                g,
                previous,
            ),
        ),
    };
    ResourceStatus {
        observed_generation: g,
        conditions: vec![accepted, programmed],
    }
}

async fn patch_status(
    client: &Client,
    ns: &str,
    key: &SourceKey,
    status: &ResourceStatus,
) -> anyhow::Result<()> {
    let patch = Patch::Merge(json!({"status": status}));
    let params = PatchParams::default();
    let name = key.name.as_str();
    let result = match key.kind {
        Kind::ProviderKey => Api::<ProviderKey>::namespaced(client.clone(), ns)
            .patch_status(name, &params, &patch)
            .await
            .map(|_| ()),
        Kind::Model => Api::<Model>::namespaced(client.clone(), ns)
            .patch_status(name, &params, &patch)
            .await
            .map(|_| ()),
        Kind::APIKey => Api::<APIKey>::namespaced(client.clone(), ns)
            .patch_status(name, &params, &patch)
            .await
            .map(|_| ()),
    };
    match result {
        Ok(()) => Ok(()),
        // Deleted between the read and the write.
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e).with_context(|| format!("updating status of {key}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::apps::v1::{DeploymentSpec, DeploymentStatus};
    use k8s_openapi::api::core::v1::PodTemplateSpec;

    fn deployment(hash: Option<&str>, updated: i32, available: i32) -> Deployment {
        Deployment {
            metadata: ObjectMeta {
                generation: Some(2),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(2),
                template: PodTemplateSpec {
                    metadata: Some(ObjectMeta {
                        annotations: hash.map(|h| {
                            BTreeMap::from([(HASH_ANNOTATION.to_string(), h.to_string())])
                        }),
                        ..Default::default()
                    }),
                    spec: None,
                },
                ..Default::default()
            }),
            status: Some(DeploymentStatus {
                observed_generation: Some(2),
                replicas: Some(2),
                updated_replicas: Some(updated),
                available_replicas: Some(available),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn rollout_states() {
        assert_eq!(rollout(None, "h"), Rollout::Missing);
        assert!(matches!(
            rollout(Some(&deployment(None, 2, 2)), "h"),
            Rollout::Progressing(_)
        ));
        assert!(matches!(
            rollout(Some(&deployment(Some("old"), 2, 2)), "h"),
            Rollout::Progressing(_)
        ));
        assert!(matches!(
            rollout(Some(&deployment(Some("h"), 1, 2)), "h"),
            Rollout::Progressing(_)
        ));
        assert_eq!(
            rollout(Some(&deployment(Some("h"), 2, 2)), "h"),
            Rollout::Ready
        );
    }

    fn src() -> Source {
        Source {
            kind: Kind::Model,
            name: "m".into(),
            generation: Some(3),
            created: None,
            fields: Default::default(),
        }
    }

    fn outcome(d: Disposition, error: Option<&str>) -> Outcome {
        Outcome {
            document: Some("{}".into()),
            dispositions: BTreeMap::from([(src().key(), d)]),
            error: error.map(String::from),
        }
    }

    fn get<'a>(s: &'a ResourceStatus, t: &str) -> &'a Condition {
        s.conditions.iter().find(|c| c.type_ == t).unwrap()
    }

    #[test]
    fn applied_and_rolled_out_is_programmed() {
        let s = desired_status(
            &src(),
            &outcome(Disposition::Applied, None),
            true,
            &Rollout::Ready,
            None,
        );
        assert_eq!(get(&s, "Accepted").status, "True");
        assert_eq!(get(&s, "Programmed").status, "True");
        assert_eq!(s.observed_generation, Some(3));
        assert_eq!(get(&s, "Programmed").observed_generation, Some(3));
    }

    #[test]
    fn applied_while_rolling_is_pending() {
        let s = desired_status(
            &src(),
            &outcome(Disposition::Applied, None),
            true,
            &Rollout::Progressing("x".into()),
            None,
        );
        assert_eq!(get(&s, "Programmed").reason, "Pending");
    }

    #[test]
    fn retained_is_not_accepted() {
        let s = desired_status(
            &src(),
            &outcome(Disposition::Retained("bad".into()), None),
            true,
            &Rollout::Ready,
            None,
        );
        assert_eq!(get(&s, "Accepted").status, "False");
        assert_eq!(get(&s, "Accepted").message, "bad");
        assert!(get(&s, "Programmed")
            .message
            .contains("previously accepted"));
    }

    #[test]
    fn transition_time_is_kept_when_status_is_unchanged() {
        let first = desired_status(
            &src(),
            &outcome(Disposition::Applied, None),
            true,
            &Rollout::Ready,
            None,
        );
        let second = desired_status(
            &src(),
            &outcome(Disposition::Applied, None),
            true,
            &Rollout::Ready,
            Some(&first),
        );
        assert_eq!(first, second);
    }
}
