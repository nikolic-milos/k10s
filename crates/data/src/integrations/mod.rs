//! Clients for tools and APIs the connected cluster already serves.
//!
//! Each adapter owns discovery, bounded decoding and typed outcomes for its
//! upstream API. Absence, denied access, partial results and failed reads stay
//! distinct. Requests use the shared transport; confirmed writes use its
//! interactive client and are never retried after transmission.
//!
//! Add wire-level coverage in `tests/scripted_apiserver` with the client, and
//! prove proxies, upgrades, controllers and writes against the live lab. The
//! `read` modules schedule these clients; the application adapts their values
//! to `k10s-provider`. Rendering never calls an adapter directly.

pub mod alertmanager;
pub mod argo;
pub mod cilium;
pub mod cilium_control;
pub mod cnpg;
pub mod eso;
pub mod falco;
pub mod flux;
pub mod gateway;
pub mod grafana;
pub mod harbor;
pub mod helm;
pub mod helm_reveal;
pub mod ingress;
pub mod kargo;
pub mod kyverno;
pub mod loki;
pub mod mesh;
pub mod oci;
pub mod otel;
pub mod policy;
pub mod prom;
pub mod proxies;
pub mod talos;
pub mod tetragon;
pub mod traces;
pub mod traefik;
pub mod vault;
pub mod velero;
