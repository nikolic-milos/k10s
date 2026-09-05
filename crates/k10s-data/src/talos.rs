//! Talos facts already published by the Kubernetes Node API.
//!
//! This module deliberately stops at detection and address selection. Machine
//! API credentials live in talosconfig and must not enter a Kubernetes scene,
//! while machine operations belong to the user's installed `talosctl`.

use k8s_openapi::api::core::v1::Node;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TalosNode {
    pub os_image: String,
    pub address: Option<String>,
}

pub fn detect(node: &Node) -> Option<TalosNode> {
    let status = node.status.as_ref()?;
    let os_image = status.node_info.as_ref()?.os_image.trim();
    if !contains_ascii_folded(os_image.as_bytes(), b"talos") {
        return None;
    }
    let address = status
        .addresses
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|address| address.type_ == "InternalIP")
        .or_else(|| {
            status
                .addresses
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|address| address.type_ == "ExternalIP")
        })
        .map(|address| address.address.clone())
        .filter(|address| !address.is_empty());
    Some(TalosNode {
        os_image: os_image.to_string(),
        address,
    })
}

fn contains_ascii_folded(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{NodeAddress, NodeStatus, NodeSystemInfo};

    fn node(os_image: &str, addresses: &[(&str, &str)]) -> Node {
        Node {
            status: Some(NodeStatus {
                node_info: Some(NodeSystemInfo {
                    os_image: os_image.to_string(),
                    ..Default::default()
                }),
                addresses: Some(
                    addresses
                        .iter()
                        .map(|(type_, address)| NodeAddress {
                            type_: type_.to_string(),
                            address: address.to_string(),
                        })
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn detection_uses_the_reported_os_and_prefers_the_internal_address() {
        let found = detect(&node(
            "Talos (v1.13.2)",
            &[("ExternalIP", "203.0.113.7"), ("InternalIP", "10.0.0.7")],
        ))
        .expect("Talos is named by Node status");

        assert_eq!(found.os_image, "Talos (v1.13.2)");
        assert_eq!(found.address.as_deref(), Some("10.0.0.7"));
    }

    #[test]
    fn a_linux_distribution_is_not_inferred_to_be_talos() {
        assert!(detect(&node("Arch Linux", &[("InternalIP", "10.0.0.8")])).is_none());
        assert!(detect(&Node::default()).is_none());
    }
}
