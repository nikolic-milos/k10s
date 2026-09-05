#!/usr/bin/env bash
# Lab-only H-plane CRD fixtures. k10s the product never installs these.
#
# Applies tiny CRDs (and one sample object each) so discovery can see the
# groups. No operators, no webhooks, no CNI, no Helm charts, no large images.
# Does not touch namespace g2, Traefik, or metrics-server.
#
#   KUBECONFIG=/home/Losmi/.rancher/k3s/k3s.yaml \
#     ./crates/k10s-data/tests/fixtures/hplane/apply.sh
#
# Uses the rancher kubectl by absolute path. Never puts that directory on PATH
# (BusyBox ar in it breaks Rust builds).
set -euo pipefail

KUBECTL="${K10S_LAB_KUBECTL:-/home/Losmi/.rancher/k3s/data/current/bin/kubectl}"
: "${KUBECONFIG:?set KUBECONFIG to the lab cluster}"
DIR="$(cd "$(dirname "$0")" && pwd)"
NS="k10s-hplane"

if [[ ! -x "$KUBECTL" ]]; then
  echo "kubectl is not executable: $KUBECTL" >&2
  exit 1
fi

CRDS=(
  clusterpolicies.kyverno.io
  policies.kyverno.io
  backups.velero.io
  clusters.postgresql.cnpg.io
  secretstores.external-secrets.io
  externalsecrets.external-secrets.io
  stages.kargo.akuity.io
  vaultconnections.secrets.hashicorp.com
  ciliumnetworkpolicies.cilium.io
  opentelemetrycollectors.opentelemetry.io
)

echo "namespace $NS"
"$KUBECTL" create namespace "$NS" --dry-run=client -o yaml | "$KUBECTL" apply -f - >/dev/null
"$KUBECTL" label namespace "$NS" k10s.dev/hplane-fixture=true --overwrite >/dev/null

echo "CRDs"
"$KUBECTL" apply -f "$DIR/crds" >/dev/null

for crd in "${CRDS[@]}"; do
  "$KUBECTL" wait --for=condition=Established "crd/$crd" --timeout=60s >/dev/null
done

echo "samples"
"$KUBECTL" apply -f "$DIR/samples" >/dev/null

echo "ready. groups served by CRD only; no controllers were installed."
