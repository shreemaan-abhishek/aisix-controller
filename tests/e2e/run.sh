#!/usr/bin/env bash
# End-to-end test: kind cluster, AISIX gateway from its Helm chart, the
# controller, and a mock OpenAI upstream.
#
#   tests/e2e/run.sh            # creates and deletes its own kind cluster
#   KEEP=1 tests/e2e/run.sh     # leaves the cluster up for inspection
set -euo pipefail
cd "$(dirname "$0")/../.."

CLUSTER=${CLUSTER:-aisix-controller-e2e}
NS=${NS:-aisix-e2e}
IMAGE=${IMAGE:-aisix-controller:dev}
AISIX_CHART_VERSION=${AISIX_CHART_VERSION:-1.9.0}
KEEP=${KEEP:-0}
MANIFESTS=tests/e2e/manifests
CREATED_CLUSTER=0

log() { printf '\n==> %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

dump() {
  echo "---- diagnostics ----"
  kubectl -n "$NS" get pods,deploy,secrets -o wide || true
  kubectl -n "$NS" get providerkeys,models,apikeys -o yaml || true
  kubectl -n "$NS" logs deploy/aisix-controller --tail=200 || true
  kubectl -n "$NS" logs deploy/aisix --tail=100 || true
}

cleanup() {
  local rc=$?
  if [ "$rc" -ne 0 ]; then dump; fi
  if [ "$KEEP" != 1 ] && [ "$CREATED_CLUSTER" = 1 ]; then
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  fi
  exit "$rc"
}
trap cleanup EXIT

k() { kubectl -n "$NS" "$@"; }

# wait_condition <resource> <name> <type> <status>: the condition holds for
# the resource's current generation.
wait_condition() {
  local res=$1 name=$2 type=$3 want=$4 gen got
  for _ in $(seq 1 120); do
    gen=$(k get "$res" "$name" -o jsonpath='{.metadata.generation}')
    got=$(k get "$res" "$name" -o jsonpath="{.status.conditions[?(@.type==\"$type\")].status}/{.status.conditions[?(@.type==\"$type\")].observedGeneration}")
    if [ "$got" = "$want/$gen" ]; then return 0; fi
    sleep 2
  done
  k get "$res" "$name" -o yaml
  fail "$res/$name: $type never became $want for generation $gen (last: $got)"
}

# chat <api-key>: prints "<http code> <body>" for a request through the gateway.
chat() {
  local out
  out=$(k exec curl -- curl -s -w '\n%{http_code}' \
    -H "Authorization: Bearer $1" -H 'Content-Type: application/json' \
    -d '{"model":"e2e/chat","messages":[{"role":"user","content":"hi"}]}' \
    http://aisix/v1/chat/completions 2>/dev/null) || true
  printf '%s %s\n' "${out##*$'\n'}" "${out%$'\n'*}"
}

# expect_chat <api-key> <code> [substring]: retries while the gateway rolls.
expect_chat() {
  local key=$1 code=$2 want=${3:-} out
  for _ in $(seq 1 60); do
    out=$(chat "$key")
    if [ "${out%% *}" = "$code" ] && { [ -z "$want" ] || [[ "$out" == *"$want"* ]]; }; then
      echo "ok: $out"
      return 0
    fi
    sleep 2
  done
  fail "expected $code ${want:+containing '$want'}, got: $out"
}

log "kind cluster $CLUSTER"
if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  kind create cluster --name "$CLUSTER" --wait 120s
  CREATED_CLUSTER=1
fi
kubectl config use-context "kind-$CLUSTER" >/dev/null

log "build and load $IMAGE"
docker build -t "$IMAGE" .
kind load docker-image "$IMAGE" --name "$CLUSTER"

log "install CRDs and controller"
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -
kubectl apply --server-side -k deploy/crds
kubectl wait --for=condition=Established crd/providerkeys.aisix.api7.ai crd/models.aisix.api7.ai crd/apikeys.aisix.api7.ai --timeout=60s
k apply -k deploy/controller
k set image deploy/aisix-controller controller="$IMAGE"
k rollout status deploy/aisix-controller --timeout=120s

log "controller seeds the resources Secret"
for _ in $(seq 1 60); do k get secret aisix-resources >/dev/null 2>&1 && break; sleep 1; done
k get secret aisix-resources >/dev/null || fail "aisix-resources Secret was not created"

log "mock upstream and curl client"
k apply -f "$MANIFESTS/mock-openai.yaml"
k run curl --image=curlimages/curl:8.11.1 --restart=Never --dry-run=client -o yaml --command -- sleep infinity | k apply -f -
k rollout status deploy/mock-openai --timeout=120s
k wait --for=condition=Ready pod/curl --timeout=120s

log "AISIX $AISIX_CHART_VERSION from its Helm chart, standalone mode"
helm repo add api7 https://charts.api7.ai >/dev/null 2>&1 || true
helm repo update api7 >/dev/null
# AISIX_CHART may point at a local chart .tgz; otherwise it is downloaded.
CHART=${AISIX_CHART:-}
if [ -z "$CHART" ]; then
  CHART_DIR=$(mktemp -d)
  CHART="$CHART_DIR/aisix-$AISIX_CHART_VERSION.tgz"
  for i in 1 2 3; do
    helm pull api7/aisix --version "$AISIX_CHART_VERSION" -d "$CHART_DIR" && break
    # The chart index points at GitHub release assets; fetch through the API instead.
    if command -v gh >/dev/null && gh release download "aisix-$AISIX_CHART_VERSION" \
        --repo api7/api7-helm-chart -p "aisix-$AISIX_CHART_VERSION.tgz" -D "$CHART_DIR" --clobber; then
      break
    fi
    [ "$i" = 3 ] && fail "could not download the aisix chart"
    sleep $((i * 5))
  done
fi
helm upgrade --install aisix "$CHART" -n "$NS" \
  --set controlPlane.enabled=false \
  --set standalone.existingSecret=aisix-resources \
  --set fullnameOverride=aisix \
  --set replicaCount=1 \
  --wait --timeout 5m

log "1. resources are applied and served"
k apply -f "$MANIFESTS/resources.yaml"
for r in providerkey/mock model/chat apikey/caller; do
  wait_condition "${r%%/*}" "${r##*/}" Accepted True
  wait_condition "${r%%/*}" "${r##*/}" Programmed True
done
expect_chat caller-e2e-key 200 'upstream-model=upstream-a auth=Bearer sk-e2e-$dollar'

log "2. an update reaches the gateway"
k patch model chat --type=merge -p '{"spec":{"model_name":"upstream-b"}}'
wait_condition model chat Programmed True
expect_chat caller-e2e-key 200 'upstream-model=upstream-b'

log "3. the CRD schema rejects unknown fields"
if k apply -f - 2>/tmp/e2e-err <<'YAML'
apiVersion: aisix.api7.ai/v1alpha1
kind: Model
metadata:
  name: bogus
spec:
  no_such_field: 1
YAML
then
  fail "unknown field was accepted"
fi
grep -q 'no_such_field' /tmp/e2e-err || fail "unexpected error: $(cat /tmp/e2e-err)"
echo "ok: $(cat /tmp/e2e-err)"

log "4. a change the gateway rejects keeps the served version"
# Passes the CRD schema; the gateway refuses a direct model that also routes.
k patch model chat --type=merge -p '{"spec":{"routing":{"targets":[{"model":"e2e/chat"}]}}}'
wait_condition model chat Accepted False
k get model chat -o jsonpath='{.status.conditions[?(@.type=="Accepted")].message}'; echo
expect_chat caller-e2e-key 200 'upstream-model=upstream-b'
k patch model chat --type=json -p '[{"op":"remove","path":"/spec/routing"}]'
wait_condition model chat Accepted True
wait_condition model chat Programmed True

log "5. a dangling reference is reported"
k patch model chat --type=merge -p '{"spec":{"provider_key_ref":{"name":"ghost"}}}'
wait_condition model chat Accepted False
k get model chat -o jsonpath='{.status.conditions[?(@.type=="Accepted")].message}' | grep -q 'ghost' \
  || fail "Accepted message does not name the missing ProviderKey"
expect_chat caller-e2e-key 200 'upstream-model=upstream-b'
k patch model chat --type=merge -p '{"spec":{"provider_key_ref":{"name":"mock"}}}'
wait_condition model chat Programmed True

log "6. deleting the APIKey revokes it"
k delete apikey caller
expect_chat caller-e2e-key 401

log "PASS"
