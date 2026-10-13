# Native observability

Choose a kubeconfig context, select a workload, and investigate telemetry
through the services that account can reach. Connection installs nothing
into the cluster. Workloads need instrumentation and matching attributes
before their metrics, logs and traces can be correlated.

## Enter a cluster

In the cluster chooser, type to filter contexts, use Up/Down and Enter. It marks
`current-context`. **Open another kubeconfig…** selects an existing file and its
contexts. Reopen with Ctrl+K, Ctrl+C or **Choose Cluster** in Ctrl+Shift+P. Failed
connections preserve the workspace; successful switches retire old cluster tabs.

The sidebar names its views. **Tools and views** or Ctrl+Alt+P opens a centered
picker with larger icons. Type a view or backend name, navigate with arrows,
PageUp/PageDown/Home/End or the wheel, then press Enter; Escape dismisses it.
Prometheus, Grafana and Loki lead to **Metrics and logs**; Tempo and Jaeger
lead to **Traces** and OpenTelemetry to **Collectors**. Trace lookup uses
Jaeger only when Tempo is absent.
Opening or filtering the picker uses cached discovery and starts no requests.
Sources not yet checked remain labelled; known absence hides the entry and
unavailable sources keep a labelled entry. Selecting one opens its native view,
where the discovery or access failure is shown in detail.

Explicit launch: `KUBECONFIG=/path/to/config k10s --cluster --context NAME`.
Add repeatable `--namespace NS` when the account needs namespace-scoped
discovery. A context identifies a cluster, user and optional namespace.
[Kubernetes kubeconfig reference](https://kubernetes.io/docs/concepts/configuration/organize-cluster-access-kubeconfig/).

From the map, `/` searches cluster objects. Confirming an object selects its
current UID, reveals it and opens the inspector. Shift+G opens **Observe**;
`i` toggles the inspector. Check context and selection.

Observe has these default shortcuts when neither query nor filter input is
active. Enter/Escape leaves input mode. Its named actions also appear under
`shell: observe ...` in Ctrl+Shift+P; restore Observe focus before invoking them.

| Action | Shortcut |
| --- | --- |
| 15-minute / 1-hour range | `1` / `2` |
| Toggle 15s / 30s resolution | `x` |
| Toggle PromQL / LogQL | `t` |
| Pod readiness | `p` |
| OTLP / conventional Pod-label logs | `o` / Shift+O |
| Open selected panel in Grafana | `g` |
| Activate log / panel table | Tab / Shift+Tab |
| Edit raw query / run it | `q` / Enter |
| Refresh | `r` |

The pane names its active table. Starting a log query selects logs; later focus
changes remain in effect when that reply arrives.

## Scope, time and samples

Scope supports Pod, Deployment, ReplicaSet, StatefulSet, DaemonSet and Job. It
verifies namespace/name/UID, then follows controlling owner UIDs to Pods, through
ReplicaSets for Deployments. Selectors narrow lookup; names and labels do not
prove ownership. Recreated objects have different UIDs. More than 64 Pods or 64
ReplicaSets, incomplete lists, denials and changed identities are labelled.
For CronJob or another unsupported kind, select a supported child.
[Kubernetes UIDs](https://kubernetes.io/docs/concepts/overview/working-with-objects/names/),
[owner references](https://kubernetes.io/docs/concepts/overview/working-with-objects/owners-dependents/).

Observe starts at a fixed 15-minute range and 15-second resolution. The toolbar
offers **15 minutes**, **1 hour**, **15s resolution** / **30s resolution**, and
**Refresh** (`r` with list focus). These re-resolve scope, advance the end time
and retain query choice. New selection clears results and resets to readiness.

Charts name source, UTC bounds, effective resolution and declared unit.
Datasource minimum intervals can increase resolution. Observe and inspector
share returned samples and bounds. **Pod readiness** reports readiness; separate
inspector CPU/memory usage and requests/limits can update while Observe stays
fixed. Usage is addressed by name or selector and is labelled **UID unverified**;
it does not establish the controlling owner or selected Pod UID. Observe has
no automatic range refresh.

Gaps remain missing. Hover names an actual sample/timestamp or no finite sample.
Association notices distinguish current Pod UIDs from missing, foreign or older
identities; query success alone proves no ownership.

## Raw queries and Grafana panels

Use `q` or **Raw query**, then Enter or **Run raw query**. **PromQL** / **LogQL**
changes language. Expressions are sent as entered: selection injects no filters
or dashboard variables. Include your source's namespace/Pod/UID labels for that
scope. Input appends text; use trailing Backspace to remove an existing query
before replacing it. Full caret, selection and paste workflows are unavailable.
[Prometheus range API](https://prometheus.io/docs/prometheus/latest/querying/api/),
[Loki range API](https://grafana.com/docs/loki/latest/reference/loki-http-api/).

Select a **Grafana panels** row and Enter or **Run panel**. Native support is one
visible PromQL range target in `timeseries` or `stat`, through its verified
Prometheus datasource UID. The resolved expression/source/unit remain visible.
Stat supports `lastNotNull` (default), `min`, `max`, `mean` and `sum` over finite
samples. Missing stays missing. `percentunit` displays as percent; other unit IDs
do not imply Grafana's full formatting.
Broken provisioned dashboard files carry their ConfigMap and filename beside
healthy catalog rows. Invalid panel arrays produce a named failure.
[Grafana datasource API](https://grafana.com/docs/grafana/latest/developer-resources/api-reference/http-api/data_source/),
[calculation types](https://grafana.com/docs/grafana/latest/visualizations/panels-visualizations/query-transform-data/calculation-types/).

| Panel variable | Native meaning and restriction |
| --- | --- |
| `$namespace` | The selected namespace. |
| `$pod` | Current scoped Pod names. Multiple Pods require `=~` or `!~`. |
| `$node` | The selected Pod's node; workload scope does not supply it. |
| `$__interval` | The largest of requested step, target min step or datasource scrape interval, and panel min interval. |
| `$__range` | The fixed range duration. |
| `$__rate_interval` | The larger of interval plus min step, and four times min step. |

Bare `${name}` also works. Namespace/Pod/node variables must fill an entire
double-quoted label value and are escaped for the matcher. Intervals must be
positive durations. Missing datasource scrape interval uses Grafana's 15-second
default; verify actual cadence before interpreting rates. Unknown variables,
formatted `${pod:regex}` and legacy `[[name]]` are unsupported.
[Grafana Prometheus variables](https://grafana.com/docs/grafana/latest/datasources/prometheus/template-variables/).

Transformations, overrides, stacking, nonlinear axes, connected nulls,
multi-target/instant targets, all-values stats and other reducers or panel/query
types carry an unsupported reason. Use raw query or **Open in Grafana**, through
a configured HTTP(S) route or Service forward. Forward failures are labelled;
check browser reachability/authentication at the destination. Opening a panel
does not prove native support.

## Related logs and traces

**OTLP logs** uses resolved namespace/Pod/UIDs. **Pod label logs** uses conventional
labels, a name link when UID is absent. Rows label the distinction. Instrumentation
and ingestion must already provide attributes. Successful empty results mean no
matching entries in that source/range.

Loki normalizes dots to underscores. Attributes can be indexed labels or
per-entry structured metadata; native correlation checks both, without assuming
UID placement.
Per-entry attributes take precedence over stream labels. Malformed or oversized
correlation attributes fail the result instead of inheriting a stream-level match.
[Loki OTel ingestion](https://grafana.com/docs/loki/latest/send-data/otel/),
[OTel Kubernetes resource attributes](https://opentelemetry.io/docs/specs/semconv/resource/k8s/).

Select a real trace ID and **Open selected trace**, or Enter with log-table focus.
Traces verifies returned ID, then span namespace/Pod/UID against captured scope.
Time/name/trace-ID links alone do not prove Kubernetes identity. The pane labels
full, partial or unproven association. Empty success keeps requested ID, clears
spans and names the empty result. Partial Tempo envelopes and invalid span
containers, IDs or timing fields fail with a named reason.
Times accept unsigned JSON integers and exactly validated quoted integer forms.
Unquoted decimal or exponent number encodings are unsupported and are refused.
Successful and empty lookups retain the actual backend and source. Jaeger is
used only when Tempo is absent; a Tempo binding failure keeps its own reason.
[Tempo lookup API](https://grafana.com/docs/tempo/latest/api_docs/).

Shift+T opens Traces; `q` edits its ID and Enter runs it. Different IDs or selection
invalidate the old link. Hiding cancels pending requests; reopen and Refresh when
asked to retry. Retained Observe results keep their fixed UTC bounds.

Select a span and press `l` or **Logs for selected span**. The query uses that
span's recorded namespace and Pod, its UID when present, and its trace ID. Its
window includes one second before and after the span. The selected Kubernetes
object does not supply missing span attributes; manual trace lookup also works
without a current selection. Missing resource attributes or time produce a
named refusal. **Refresh** preserves the recorded expression and UTC bounds.
Changing selection starts a current-workload investigation. Changing range or
resolution reruns the expression over a recent window and drops the span link.

This link selects logs for the trace during the span's window. The origin span
does not prove that each returned line belongs to that exact span. Rows distinguish
matching recorded UID from weaker resource or trace links. Opening this focused
log view does not start a Grafana catalogue or unrelated metrics query.
[Grafana trace-to-logs requirements](https://grafana.com/docs/grafana/latest/datasources/tempo/configure-tempo-data-source/configure-trace-to-logs/).

## Inspector and retained history

Inspector events are filtered by the selected object's UID. A Pod log tail
checks the selected UID before and after the request and withholds lines after
name reuse. Kubelet log requests are still addressed by name; the heading states
that the backend UID is unverified. Metadata checks alone cannot attest the
identity of the kubelet's returned container output.

Restarted containers request their previous output. An explicit missing-previous
response or the runtime refusal naming the recorded terminated container ID
permits a labelled current-container fallback. Other failures retain their own
outcome and the server's sentence. Timestamped application output remains a log
line even when its text resembles a control response.

When previous output is unavailable and Loki is discovered, **Retained logs in
Loki** opens an investigation for the selected Pod UID. Scope is verified again
before querying. This retrieves only records already delivered to Loki with the
required attributes; it does not recover output that was never collected.

## Collector inventory and delivery evidence

Shift+C opens the focused **Collectors** document. Refresh reads Collector
metadata and observed process health without fetching other ecosystem families.
Hiding cancels unfinished reads; reopening resumes an interrupted request.
Shift+E also opens **Ecosystem**, where Tab/Shift+Tab selects **OpenTelemetry**.
Collector CRs are metadata only: config/image/readiness are not fetched.
Discovered standalone Services show health_check or zpages endpoint results. Absence, denial,
failure and process health stay distinct.

Health proves the endpoint answered. Delivery requires actual producer
metric/log/trace records with intended identity at the destinations. Components
must be enabled in service pipelines. k10s installs no instrumentation/backends.
[Collector configuration](https://opentelemetry.io/docs/collector/configuration/),
[Collector 0.160.0 health extension](https://raw.githubusercontent.com/open-telemetry/opentelemetry-collector-contrib/v0.160.0/extension/healthcheckextension/README.md).

Helm release inventory uses Secret metadata labels for name, revision and status.
Stored chart details, deployment timestamps, values and manifests are not read.
Reveal, revision diff and stored-revision apply report this limitation before
any request. Use the existing Helm tool when an operation requires its payloads.

## Current limits

- Queries accept 8 KiB expressions. Prometheus retains at most 256 series and
  5,000 points each; charts draw 16, naming omissions. Histograms are refused.
- Observe log requests ask for 200 entries. Loki decoding is bounded to 16
  streams and 8 KiB per displayed line; clipping and truncation are labelled.
- Inspector tails request 200 lines and 64 KiB from kubelet. Successful response
  bodies also have a local 64 KiB cap; oversized bodies are refused before lines
  or retained-history permission appear. The SDK still buffers HTTP error bodies
  while preserving their structured status and server sentence.
  Inspector events and tails, and focused Collector reads, share two active
  and four waiting requests. Their 35-second deadline covers queue time and
  the full exchange.
- Traces retain at most 4,096 spans; timeline shows eight rows. Use the table
  for other retained spans.
- Labels/notices show abbreviation. Chart/catalog notices show at most eight
  messages, 512 bytes each and 4 KiB total, with omission counts. Full chart
  notices remain retained; there is no full-text expansion control.
- Competing source Services are unresolved; Observe has no source picker.
  Discovery can lag new competition by 30 seconds or until Refresh.
  Verification/query are not atomic.
- Arrows/PageUp/PageDown/Home/End navigate; `/` filters. Enter/Escape leave
  typing/filtering. Tab selects logs and Shift+Tab selects panels; click/scroll
  also chooses focus. Ctrl+Tab or Ctrl+Shift+Tab switches tabs.
- Both `k10s-dark` and `k10s-light` themes exist. Compact shell density is not
  implemented. `--layout dense` changes map placement, not shell spacing.
