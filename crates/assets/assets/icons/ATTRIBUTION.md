# Icon attribution

`deploy.svg`, `sts.svg`, `ds.svg`, `job.svg` are derived from the official
Kubernetes resource icons, <https://github.com/kubernetes/community>
(`icons/svg/resources/unlabeled/`), © the Kubernetes Authors, licensed
CC BY 4.0 <https://creativecommons.org/licenses/by/4.0/>.

Two kinds share one file where the upstream set has no separate glyph:
CronJob is drawn with `job.svg` and Ingress with `svc.svg` (see `KIND_GLYPHS`
in `src/lib.rs`), so this notice covers both uses of each file.

Modifications: Inkscape/RDF metadata removed, the opaque `#326ce5` background
hexagon and its outline dropped, and remaining paths collapsed to a plain white
fill. The original resource glyph remains; gpui rasterizes it as a monochrome
alpha mask tinted at paint time. The native card supplies the resource boundary.

`pvc.svg`, `svc.svg`, `cm.svg`, `secret.svg` (satellite glyphs) and
`unknown.svg` (the fallback drawn for any kind or vendor with no compiled-in
glyph, including CRDs) are original k10s minimal masks, not derived from the
Kubernetes icon set; no attribution required.

`tools/*.svg` are unmodified brand icons from the simple-icons project
(v16.27.0), <https://github.com/simple-icons/simple-icons>, released under
CC0 1.0, except the original masks named below. All trademarks and project
names the `tools/` icons identify - whether drawn from simple-icons or as
original k10s masks - remain the property of their respective owners; k10s
uses them only to identify workloads running those tools, with no
affiliation or endorsement implied.

`headlamp.svg`, `kargo.svg`, `kyverno.svg`, `loki.svg`, `tetragon.svg` and
`velero.svg` (under `tools/`) are original k10s minimal monochrome masks,
not derived from those projects' logos; no attribution required. Those
slugs were absent from simple-icons 16.27.0.

`chart-no-axes-combined.svg` and `route.svg` are unmodified Lucide 1.15.0
icons, <https://github.com/lucide-icons/lucide/tree/1.15.0/icons>. They identify
metrics and trace workflows in native navigation. The ISC notice is included
in `lucide-ISC.txt`.
