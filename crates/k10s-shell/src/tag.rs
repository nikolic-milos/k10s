//! What identifies one hosted item, and what a cluster switch does to it.
//!
//! A tag is how the workspace finds an item it already has open, so asking for
//! the same document twice activates the tab rather than stacking a copy of the
//! same question. It is also the whole of the answer to "does this survive a
//! cluster switch", which is why [`ItemTag::on_adopt`] lives here next to the
//! kinds rather than beside the code that switches: a tag and its fate are one
//! decision.

/// What a cluster switch does to one open item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnAdopt {
    /// Everything in it came out of the cluster and none of it is the user's, so
    /// the previous cluster's answers leave with the previous cluster.
    Retire,
    /// Cluster-derived, but holding text a person may have typed. Discarding
    /// unsaved work to keep a provider tidy is the wrong trade, and the slot means
    /// its next apply reaches the cluster this window is actually on.
    KeepUnsavedWork,
    /// Nothing in it belongs to any cluster: the map's own scene, the file tree, a
    /// local shell.
    NotTheClusters,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemTag {
    Map,
    Browse,
    Nodes,
    Forwards,
    Files,
    Releases,
    Doc(String),
    Edit(String),
    Diff(String),
    Logs(String),
    Term(String),
    LocalTerm,
}

impl ItemTag {
    // The answer lives on the kind, next to the kind, and is a `match` with no
    // wildcard arm.
    //
    // It used to be a `matches!` beside `adopt`, twelve hundred lines from here.
    // An unlisted variant fell through to "not cluster bound", which is the
    // dangerous default: a cluster-backed tab that survives the switch goes on
    // painting the cluster the window has left, under a title that names no
    // cluster at all -- the one failure nothing on screen would admit to. The
    // hand-written guard test could not catch it either, because a kind nobody
    // added to the list is a kind neither of its loops ever constructs, so both
    // passed. Here a kind that has not answered does not compile, and that is the
    // whole reason for the shape.
    pub fn on_adopt(&self) -> OnAdopt {
        match self {
            ItemTag::Browse
            | ItemTag::Nodes
            | ItemTag::Forwards
            | ItemTag::Releases
            | ItemTag::Doc(_)
            | ItemTag::Diff(_)
            | ItemTag::Logs(_)
            | ItemTag::Term(_) => OnAdopt::Retire,
            ItemTag::Edit(_) => OnAdopt::KeepUnsavedWork,
            ItemTag::Map | ItemTag::Files | ItemTag::LocalTerm => OnAdopt::NotTheClusters,
        }
    }

    pub fn retires_on_adopt(&self) -> bool {
        self.on_adopt() == OnAdopt::Retire
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Switching cluster invalidates every view whose content came out of the old
    // one. What it must *not* invalidate is anything holding text a person typed.
    //
    // Completeness is the compiler's half now: `on_adopt` is a match with no
    // wildcard arm, so a kind that has not answered does not build, and this test
    // no longer has to be the thing that notices. What it pins is the answers --
    // that nothing cluster-backed drifts into being kept, and that the two
    // reasons for keeping stay apart, because collapsing them is how the editor's
    // exemption would later read as an oversight and get "tidied" into a retire.
    #[test]
    fn a_cluster_switch_retires_the_views_it_invalidates_and_keeps_the_rest() {
        for (tag, expected) in [
            (ItemTag::Browse, OnAdopt::Retire),
            (ItemTag::Nodes, OnAdopt::Retire),
            (ItemTag::Forwards, OnAdopt::Retire),
            (ItemTag::Releases, OnAdopt::Retire),
            (ItemTag::Doc("uid/name".into()), OnAdopt::Retire),
            (ItemTag::Diff("uid/name".into()), OnAdopt::Retire),
            (ItemTag::Logs("prod/pod-1".into()), OnAdopt::Retire),
            (ItemTag::Term("prod/pod-1".into()), OnAdopt::Retire),
            (ItemTag::Map, OnAdopt::NotTheClusters),
            (ItemTag::Files, OnAdopt::NotTheClusters),
            (ItemTag::LocalTerm, OnAdopt::NotTheClusters),
            (
                ItemTag::Edit("cluster:uid/name".into()),
                OnAdopt::KeepUnsavedWork,
            ),
            (
                ItemTag::Edit("file:/tmp/x.yaml".into()),
                OnAdopt::KeepUnsavedWork,
            ),
            (ItemTag::Edit(String::new()), OnAdopt::KeepUnsavedWork),
        ] {
            assert_eq!(tag.on_adopt(), expected, "{tag:?}");
            assert_eq!(
                tag.retires_on_adopt(),
                expected == OnAdopt::Retire,
                "{tag:?}"
            );
        }
    }

    #[test]
    fn two_documents_are_the_same_item_only_when_they_name_the_same_resource() {
        // The tag is the dedup key, so equality is what stops a second describe
        // of the same object opening beside the first -- and what stops two
        // different objects collapsing into one tab.
        assert_eq!(
            ItemTag::Doc("uid-1/api".into()),
            ItemTag::Doc("uid-1/api".into())
        );
        assert_ne!(
            ItemTag::Doc("uid-1/api".into()),
            ItemTag::Doc("uid-2/api".into())
        );
        assert_ne!(
            ItemTag::Doc("uid-1/api".into()),
            ItemTag::Edit("uid-1/api".into()),
            "describing a resource and editing it are two items, not one"
        );
        assert_ne!(
            ItemTag::Term("prod/pod-1".into()),
            ItemTag::LocalTerm,
            "a shell in a container is not the local shell"
        );
    }
}
