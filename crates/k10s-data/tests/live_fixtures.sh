#!/usr/bin/env bash
# Create everything `live_cluster.rs` expects, in the namespace it reads.
#
# This exists because the module comment's prose version of it was incomplete in
# four ways, each of which fails one assertion a long way from the missing
# fixture: the ConfigMap needs a label and a specific data key, and two Secrets
# are needed that the comment never mentioned. A setup nobody can reproduce is a
# suite that passed once, on the machine of the person who wrote it.
#
#   KUBECONFIG=/path/to/kubeconfig ./crates/k10s-data/tests/live_fixtures.sh
#
# Then, and note the thread count, which is not optional:
#
#   KUBECONFIG=/path/to/kubeconfig cargo test -p k10s-data --test live_cluster \
#     -- --ignored --nocapture --test-threads=1
#
# Two of these objects are created with a *client-side* apply and must stay that
# way. `kubectl apply` writes the object it sent into a
# `last-applied-configuration` annotation, and that annotation is the base
# document of the three-way diff; server-side apply does not write one, so a
# fixture created with `--server-side` silently turns two tests into two-way
# comparisons that still pass for the wrong reason.
set -euo pipefail

NS="${K10S_LIVE_NAMESPACE:-g2}"
READER_GROUP="${K10S_LIVE_READER_GROUP:-k10s-readers}"
KUBECTL=(kubectl)
command -v kubectl >/dev/null || { echo "kubectl is not on PATH" >&2; exit 1; }
: "${KUBECONFIG:?set KUBECONFIG to the cluster to set up}"

echo "namespace $NS"
"${KUBECTL[@]}" create namespace "$NS" --dry-run=client -o yaml | "${KUBECTL[@]}" apply -f - >/dev/null

# The identity the write-denial test needs: it may read everything and patch
# nothing, so the refusal it gets is the server's own rather than a client guess.
echo "reader RBAC for group $READER_GROUP"
"${KUBECTL[@]}" apply -f - >/dev/null <<YAML
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: { name: k10s-reader }
rules:
- apiGroups: ["", "apps", "apiextensions.k8s.io", "policy", "metrics.k8s.io", "k10s.test"]
  resources: ["*"]
  verbs: ["get", "list", "watch"]
- apiGroups: ["authorization.k8s.io"]
  resources: ["selfsubjectaccessreviews", "selfsubjectrulesreviews"]
  verbs: ["create"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata: { name: k10s-reader }
roleRef: { apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: k10s-reader }
subjects:
- { kind: Group, name: $READER_GROUP, apiGroup: rbac.authorization.k8s.io }
YAML

# Client-side, for the annotation. `team: platform` proves a label that is not
# apply bookkeeping survives into the edited document; `greeting: hello` is read
# out of the base to prove the base rendered through the same emitter.
echo "configmap settings (client-side apply)"
"${KUBECTL[@]}" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: settings
  namespace: $NS
  labels:
    team: platform
data:
  greeting: hello
  retries: "3"
  timeout: "30s"
YAML

echo "deployment web"
"${KUBECTL[@]}" apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: { name: web, namespace: $NS }
spec:
  replicas: 1
  selector: { matchLabels: { app: web } }
  template:
    metadata: { labels: { app: web } }
    spec:
      containers:
      - { name: web, image: nginx:1.27-alpine }
YAML

# Route one of the Secret rule: a value in the object itself.
echo "secret api-token"
"${KUBECTL[@]}" -n "$NS" create secret generic api-token \
  --from-literal=token=super-secret-value \
  --dry-run=client -o yaml | "${KUBECTL[@]}" apply -f - >/dev/null

# Route two, and the subtle one: client-side apply puts the *declared* value in
# an annotation, and annotations are `ObjectMeta`, so it survives the
# metadata-only fetch that keeps route one safe.
echo "secret declared-token (client-side apply, plaintext in the annotation)"
"${KUBECTL[@]}" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Secret
metadata:
  name: declared-token
  namespace: $NS
type: Opaque
stringData:
  token: plaintext-in-the-annotation
YAML

# A kind this binary has never heard of. The printer columns are deliberate: a
# client falling back to its own rendering would still produce a table, and only
# the CRD author's own column names tell the two apart.
echo "CRD widgets.k10s.test and one widget"
"${KUBECTL[@]}" apply -f - >/dev/null <<'YAML'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: { name: widgets.k10s.test }
spec:
  group: k10s.test
  scope: Namespaced
  names: { plural: widgets, singular: widget, kind: Widget }
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            properties:
              size: { type: integer }
              flavour: { type: string }
            required: [size]
    additionalPrinterColumns:
    - { name: Size, type: integer, jsonPath: .spec.size }
    - { name: Flavour, type: string, jsonPath: .spec.flavour }
YAML
# The CRD has to be established before an instance of it will be accepted.
"${KUBECTL[@]}" wait --for=condition=Established crd/widgets.k10s.test --timeout=60s >/dev/null
"${KUBECTL[@]}" apply -f - >/dev/null <<YAML
apiVersion: k10s.test/v1
kind: Widget
metadata: { name: sprocket, namespace: $NS }
spec: { size: 7, flavour: vanilla }
YAML

echo
echo "ready. One caveat that is not cosmetic:"
echo "  run the suite with --test-threads=1. The field-manager conflict test"
echo "  leaves a manager named 'rival' owning .data.retries on this ConfigMap,"
echo "  and in parallel that reaches the staleness test as a conflict instead."
