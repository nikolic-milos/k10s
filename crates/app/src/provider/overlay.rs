//! Translate overlay data and network policy posture into provider outcomes.

use k10s_data::read::Fetched;

pub(super) fn overlay_outcome(
    fetched: Fetched<k10s_data::overlay::Frame>,
) -> k10s_provider::OverlayOutcome {
    match fetched {
        Fetched::Ok(frame) => k10s_provider::OverlayOutcome::Ready {
            stamps: frame
                .stamps
                .into_iter()
                .map(|stamp| k10s_provider::OverlayStamp {
                    uid: stamp.uid,
                    namespace: stamp.namespace,
                    name: stamp.name,
                    tint: stamp.tint,
                    samples: stamp.samples,
                    label: stamp.label,
                })
                .collect(),
            truncated: frame.truncated,
            note: frame.note,
        },
        Fetched::Denied { what } => k10s_provider::OverlayOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::OverlayOutcome::Failed(why),
    }
}

pub(super) fn posture_outcome(
    fetched: Fetched<k10s_data::netpol::PodInspection>,
) -> k10s_provider::PostureOutcome {
    match fetched {
        Fetched::Ok(inspection) if !inspection.found => k10s_provider::PostureOutcome::Missing,
        Fetched::Ok(inspection) => {
            let Some(posture) = inspection.posture else {
                return k10s_provider::PostureOutcome::Missing;
            };
            k10s_provider::PostureOutcome::Ready(k10s_provider::PodPostureView {
                ingress_isolated: posture.ingress.isolated,
                ingress_policies: posture.ingress.selecting_policies,
                ingress_names: posture.ingress.policies,
                ingress_truncated: posture.ingress.policies_truncated,
                egress_isolated: posture.egress.isolated,
                egress_policies: posture.egress.selecting_policies,
                egress_names: posture.egress.policies,
                egress_truncated: posture.egress.policies_truncated,
                ports: inspection
                    .ports
                    .into_iter()
                    .map(|port| {
                        format!(
                            "{} {} {}",
                            port.name,
                            match port.protocol {
                                k10s_data::netpol::Protocol::Tcp => "TCP",
                                k10s_data::netpol::Protocol::Udp => "UDP",
                                k10s_data::netpol::Protocol::Sctp => "SCTP",
                            },
                            port.port
                        )
                    })
                    .collect(),
                completeness: completeness_line(posture.completeness),
            })
        }
        Fetched::Denied { what } => k10s_provider::PostureOutcome::Denied(what),
        Fetched::Failed { why, .. } => k10s_provider::PostureOutcome::Failed(why),
    }
}

fn completeness_line(completeness: k10s_data::netpol::Completeness) -> String {
    use k10s_data::netpol::Completeness;
    match completeness {
        Completeness::Complete => String::new(),
        Completeness::Truncated {
            evaluated_policies,
            total_policies,
        } => format!("policy set truncated; evaluated {evaluated_policies} of {total_policies}"),
        Completeness::IncompleteInventory {
            policies,
            pods,
            namespaces,
        } => {
            let mut parts = Vec::new();
            if policies {
                parts.push("policies");
            }
            if pods {
                parts.push("pods");
            }
            if namespaces {
                parts.push("namespaces");
            }
            format!("inventory incomplete ({})", parts.join(", "))
        }
    }
}
