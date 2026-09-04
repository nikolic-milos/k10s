//! What every ecosystem adapter asks a served API group before it lists
//! anything, and how it reads the answer: whether the group is served, denied
//! or missing, which versions to try in what order, where a group lives, and
//! what a list error means. One copy, so the sixteen adapters that read a
//! group the same way cannot drift apart.

/// What discovery said about a CRD group.
pub(crate) enum GroupAnswer {
    Served(Vec<String>),
    NotServed,
    Denied,
    Failed(String),
}

/// What a list against a served group came back with when it did not come
/// back with a list.
pub(crate) enum ListErr {
    NotFound,
    Denied,
    Failed(String),
}

pub(crate) fn after_group(error: &kube::Error) -> GroupAnswer {
    if let kube::Error::Api(response) = error {
        if matches!(response.code, 401 | 403) {
            return GroupAnswer::Denied;
        }
        if response.code == 404 {
            return GroupAnswer::NotServed;
        }
    }
    GroupAnswer::Failed(crate::connect::describe(
        error as &(dyn std::error::Error + 'static),
    ))
}

pub(crate) fn after_list(error: &kube::Error) -> ListErr {
    if let kube::Error::Api(response) = error {
        if matches!(response.code, 401 | 403) {
            return ListErr::Denied;
        }
        if response.code == 404 {
            return ListErr::NotFound;
        }
    }
    ListErr::Failed(crate::connect::describe(
        error as &(dyn std::error::Error + 'static),
    ))
}

pub(crate) fn order_versions(preferred: &str, versions: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    if !preferred.is_empty() {
        out.push(preferred.to_string());
    }
    for version in versions {
        if version.is_empty() || out.iter().any(|have| have == &version) {
            continue;
        }
        out.push(version);
    }
    out
}

pub(crate) fn group_url(group: &str) -> String {
    format!("/apis/{group}")
}
