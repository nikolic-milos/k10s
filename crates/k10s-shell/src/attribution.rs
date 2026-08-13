//! Third-party notices that must travel with the binary.
//!
//! Workload kind glyphs derive from the Kubernetes icon set under CC BY 4.0.
//! That license requires the attribution to accompany the work, so this
//! document is compiled in rather than left next to the source. Print it with
//! `k10s --attribution`.

/// The notice as shipped. Identical to `crates/k10s-map/assets/icons/ATTRIBUTION.md`.
pub const DOCUMENT: &str = include_str!("../../k10s-map/assets/icons/ATTRIBUTION.md");

/// SVG filenames named in a CC BY paragraph of `document`.
///
/// Original k10s masks and CC0 brand icons are listed in the same file and
/// are not returned: they are not CC BY works.
pub fn cc_by_icons(document: &str) -> Vec<&str> {
    let mut pending = Vec::new();
    let mut found = Vec::new();
    for line in document.lines() {
        collect_svg_names(line, &mut pending);
        if line.contains("CC BY") {
            found.append(&mut pending);
        }
        if line.is_empty() {
            pending.clear();
        }
    }
    found
}

fn collect_svg_names<'a>(line: &'a str, out: &mut Vec<&'a str>) {
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        let name = &rest[..end];
        rest = &rest[end + 1..];
        if name.ends_with(".svg") && !name.contains('/') && !name.contains('*') {
            out.push(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cc_by_paragraph_yields_its_svg_names_and_ignores_the_rest() {
        let document = "\
`a.svg`, `b.svg` from somewhere, licensed
CC BY 4.0 <https://creativecommons.org/licenses/by/4.0/>.

`c.svg` is original; no attribution required.

`d.svg` from simple-icons, CC0 1.0.
";
        assert_eq!(cc_by_icons(document), ["a.svg", "b.svg"]);
    }

    #[test]
    fn the_shipped_notice_lists_the_kubernetes_cc_by_glyphs() {
        let icons = cc_by_icons(DOCUMENT);
        for name in ["deploy.svg", "sts.svg", "ds.svg", "job.svg"] {
            assert!(
                icons.contains(&name),
                "{name} is a CC BY derivative and must be named; got {icons:?}"
            );
        }
        assert!(
            DOCUMENT.contains("Kubernetes Authors"),
            "CC BY requires the copyright holder"
        );
        assert!(
            DOCUMENT.contains("https://creativecommons.org/licenses/by/4.0/"),
            "CC BY requires a link to the license"
        );
    }

    #[test]
    fn original_and_cc0_icons_are_not_listed_as_cc_by() {
        let icons = cc_by_icons(DOCUMENT);
        for name in ["pvc.svg", "svc.svg", "cm.svg", "secret.svg", "unknown.svg"] {
            assert!(
                !icons.contains(&name),
                "{name} is an original k10s mask, not a CC BY work: {icons:?}"
            );
        }
        assert!(
            !icons.iter().any(|name| name.contains("tools/")),
            "simple-icons are CC0, not CC BY: {icons:?}"
        );
        assert!(
            DOCUMENT.contains("CC0 1.0"),
            "the notice must still name the CC0 tool logos so they are not mistaken for CC BY"
        );
    }
}
