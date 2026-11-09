# aisix-controller

A Kubernetes controller for the [AISIX](https://github.com/api7/aisix) AI gateway. It reconciles AISIX custom resources into the gateway's standalone resources file, so models, provider keys and caller API keys are managed with `kubectl` and GitOps.

v0.1 covers the request path: `ProviderKey`, `Model` and `APIKey`.

## How it works

```
ProviderKey / Model / APIKey + Secrets
        │ watch
        ▼
aisix-controller ── render ── validate with the gateway's own pipeline
        │
        ├─► Secret aisix-resources   (resources.yaml, standalone format v1)
        └─► Deployment aisix         (pod-template hash annotation → rollout)
```

1. The controller watches the three resource kinds, the Secrets they reference, and the gateway Deployment in its namespace.
2. It renders every resource into one `resources.yaml` and checks it with `aisix_core::filesource::load_from_str`, the same code the gateway runs at boot.
3. A resource the gateway would reject is reported on its status, and the version currently served stays in the file. A resource with no served version is left out. The others still apply.
4. When the document changes, the controller writes it to the gateway's resources Secret and stamps its hash on the Deployment's pod template, which rolls the pods.

Status conditions on every resource:

| Condition | True when |
|---|---|
| `Accepted` | The current spec is valid. |
| `Programmed` | The gateway serves the current spec (the rollout carrying it finished). |

## Resources

The spec is the AISIX resource document, with the gateway's own field names. Credentials and references are replaced by Kubernetes references:

| Kind | Replaced field | Kubernetes field |
|---|---|---|
| `ProviderKey` | `api_key` / `secret` | `api_key_secret_ref: {name, key}` |
| `Model` | `provider_key_id` | `provider_key_ref: {name}` (a ProviderKey's `metadata.name`) |
| `APIKey` | `key_hash` | `key_secret_ref: {name, key}` (plaintext; the controller writes only its SHA-256) |

`display_name` defaults to `metadata.name`. Set it when the AISIX name is not a valid Kubernetes name, for example a model called `openai/gpt-4o`. Id-form references (`model_id`, `allowed_model_ids`, ...) are not accepted; use the name forms.

```yaml
apiVersion: aisix.api7.ai/v1alpha1
kind: ProviderKey
metadata:
  name: openai
spec:
  provider: openai
  api_key_secret_ref: {name: llm-creds, key: openai}
---
apiVersion: aisix.api7.ai/v1alpha1
kind: Model
metadata:
  name: gpt-4o
spec:
  provider: openai
  model_name: gpt-4o
  provider_key_ref: {name: openai}
---
apiVersion: aisix.api7.ai/v1alpha1
kind: APIKey
metadata:
  name: team-a
spec:
  allowed_models: ["gpt-4o"]
  key_secret_ref: {name: caller-keys, key: team-a}
```

`kubectl get providerkeys,models.aisix.api7.ai,apikeys` shows `Accepted` and `Programmed` columns.

## Schemas

The CRD schemas in `deploy/crds` are generated from aisix-core's strict resource schemas (`resource_root_schema`), pinned by the `aisix-core` git tag in `Cargo.toml`:

```bash
make crds        # regenerate
make crds-check  # CI: fail when deploy/crds is stale
```

The generator converts aisix-core's draft-07 schemas to structural OpenAPI. Types, required fields, enums, ranges, lengths and patterns carry over. Cross-field rules that a structural schema cannot express (for example "a direct model cannot also route") are dropped from the CRD and enforced by the controller's validation step, which reports them on `Accepted`.

## Install

The controller and the gateway run in the same namespace. Install the controller first, so the resources Secret exists when the gateway starts.

```bash
kubectl create namespace aisix
kubectl apply --server-side -k deploy/crds
kubectl -n aisix apply -k deploy/controller   # set the image with a kustomize overlay

helm repo add api7 https://charts.api7.ai
helm install aisix api7/aisix -n aisix \
  --set controlPlane.enabled=false \
  --set standalone.existingSecret=aisix-resources \
  --set fullnameOverride=aisix
```

Controller flags: `--namespace` (defaults to its own), `--secret-name` (`aisix-resources`), `--secret-key` (`resources.yaml`), `--deployment-name` (`aisix`), `--resync-seconds` (`60`).

## Development

```bash
make test    # unit tests
make lint    # fmt + clippy
make e2e     # kind cluster + AISIX chart + controller + mock upstream
KEEP=1 make e2e   # keep the cluster afterwards
```

The e2e suite needs Docker, kind, kubectl and helm. It covers: resources served end to end (including a `$` in a credential), updates, CRD field validation, a gateway-rejected change keeping the served version, a dangling reference, and key revocation.

## v0.1 limitations

- Every configuration change rolls the gateway pods, because standalone AISIX reloads only on SIGHUP. Node-local rate-limit counters reset on each rollout. A push endpoint in AISIX would remove this.
- One namespace, one gateway Deployment and one controller replica (no leader election).
- Three kinds. Guardrails, MCP servers, A2A agents, cache and rate-limit policies, OIDC providers, claim mappings, passthrough routes and exporters come next.
- No admission webhook: invalid resources are accepted by the API server and reported on status.
