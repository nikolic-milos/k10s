#!/usr/bin/env bash
# Remove the lab-only H-plane CRD fixtures applied by apply.sh.
#
# Deletes sample objects, then the CRDs, then namespace k10s-hplane.
# Idempotent. Does not touch namespace g2, Traefik, metrics-server,
# widgets.k10s.test, or any object without k10s.dev/hplane-fixture.
#
#   KUBECONFIG=/home/Losmi/.rancher/k3s/k3s.yaml \
#     ./crates/k10s-data/tests/fixtures/hplane/revert.sh
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

echo "samples"
# Missing kinds (CRDs already gone) must not fail a second run.
"$KUBECTL" delete -f "$DIR/samples" --ignore-not-found --wait=false >/dev/null 2>&1 || true

echo "CRDs"
"$KUBECTL" delete crd "${CRDS[@]}" --ignore-not-found --wait=false >/dev/null
"$KUBECTL" delete crd -l k10s.dev/hplane-fixture=true --ignore-not-found --wait=false >/dev/null

echo "namespace $NS"
"$KUBECTL" delete namespace "$NS" --ignore-not-found --wait=false >/dev/null

echo "reverted."
