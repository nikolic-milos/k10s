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
#   KUBECONFIG=/path/to/kubeconfig cargo test -p k10s-data --test live_adapters \
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

# The containerPort is not decoration: port-forward resolves a pod's port from
# it, and without one k10s refuses by name -- "declares no containerPort" --
# which is correct behaviour and a broken fixture.
echo "deployment web (with a declared containerPort, which port-forward needs)"
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
      - name: web
        image: nginx:1.27-alpine
        ports:
        - containerPort: 80
YAML

# A second workload on a HIGH port, which port-forward needs and `web` cannot
# provide. k10s picks the local port from the container's own, so a pod on 80
# resolves to a local 80 that no unprivileged process may bind -- the forward
# then reports `Dead { why: "... Permission denied" }`, correctly and uselessly.
# This one serves a known string so the test can prove the bytes came from that
# container rather than from anything listening locally.
echo "deployment forward-probe (high port, for port-forward)"
"${KUBECTL[@]}" apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: { name: forward-probe, namespace: $NS }
spec:
  replicas: 1
  selector: { matchLabels: { app: forward-probe } }
  template:
    metadata: { labels: { app: forward-probe } }
    spec:
      containers:
      - name: probe
        image: busybox:1.36
        command:
        - sh
        - -c
        - mkdir -p /www && echo k10s-forward-probe > /www/index.html && httpd -f -p 18081 -h /www
        ports:
        - containerPort: 18081
YAML

# The usage tests' subject: a pod that declares both requests and limits, so
# "usage against its requests and limits" has four declared numbers to land
# on. The workload does close to nothing on purpose -- the assertions are
# about presence and provenance, not about magnitude.
echo "deployment usage-probe (declared requests and limits, for the usage tests)"
"${KUBECTL[@]}" apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata: { name: usage-probe, namespace: $NS }
spec:
  replicas: 1
  selector: { matchLabels: { app: usage-probe } }
  template:
    metadata: { labels: { app: usage-probe } }
    spec:
      containers:
      - name: probe
        image: busybox:1.36
        command: ["sh", "-c", "while true; do sleep 3600; done"]
        resources:
          requests: { cpu: 10m, memory: 16Mi }
          limits: { cpu: 100m, memory: 64Mi }
YAML

# The identity the metrics-denial test needs: it may read everything the
# reader may EXCEPT metrics.k8s.io, so a usage poll under it gets the
# server's own 403 on pod metrics and nothing else is different. Bound to a
# ServiceAccount rather than a group so a lab can mint a token for it without
# certificate ceremony; the kubeconfig context wiring stays outside this
# script, exactly like reader@k10s-lab's.
echo "no-metrics RBAC (ClusterRole k10s-reader-nometrics, ServiceAccount $NS/nometrics)"
"${KUBECTL[@]}" apply -f - >/dev/null <<YAML
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: { name: k10s-reader-nometrics }
rules:
- apiGroups: ["", "apps", "apiextensions.k8s.io", "policy", "k10s.test"]
  resources: ["*"]
  verbs: ["get", "list", "watch"]
- apiGroups: ["authorization.k8s.io"]
  resources: ["selfsubjectaccessreviews", "selfsubjectrulesreviews"]
  verbs: ["create"]
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: nometrics, namespace: $NS }
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata: { name: k10s-reader-nometrics }
roleRef: { apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: k10s-reader-nometrics }
subjects:
- { kind: ServiceAccount, name: nometrics, namespace: $NS }
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

# Accepted is not running. Exec, port-forward and the usage poll all need a
# started container, and without this wait the first run after a fixture
# refresh races the image pull -- which fails as a timeout in a test that
# names something else entirely.
echo "waiting for the pods those tests exec into, forward to, and measure"
for deployment in web forward-probe usage-probe; do
  "${KUBECTL[@]}" -n "$NS" rollout status "deployment/$deployment" --timeout=180s >/dev/null
done

echo
echo "ready. One caveat that is not cosmetic:"
echo "  run the suite with --test-threads=1. The field-manager conflict test"
echo "  leaves a manager named 'rival' owning .data.retries on this ConfigMap,"
echo "  and in parallel that reaches the staleness test as a conflict instead."
