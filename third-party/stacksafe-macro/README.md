# `stacksafe-macro` compatibility backport

GPUI currently resolves `stacksafe 0.1.4`, which requires
`stacksafe-macro =0.1.4`. The published macro depends on
`proc-macro-error2 2.0.1`, whose private `proc_macro` re-export is rejected by
a future Rust release.

This package preserves the required name, version and expansion while
backporting the parser from the upstream Apache-2.0-licensed
`stacksafe-macro 1.0.3`. The newer parser reports errors directly through
`syn` and has no `proc-macro-error2` dependency.

Upstream source:
<https://github.com/fast/stacksafe/tree/stacksafe-macro-v1.0.3/stacksafe-macro>

Remove this patch once GPUI accepts `stacksafe >=1.0`.
