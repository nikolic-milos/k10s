use super::PanelQuery;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variables {
    pub namespace: String,
    pub pods: Vec<String>,
    pub node: Option<String>,
    pub start_ms: i64,
    pub end_ms: i64,
    pub step_ms: u64,
    pub min_interval: String,
}

pub fn duration_ms(text: &str) -> Result<u64, String> {
    let mut rest = text.trim();
    let mut total = 0u64;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(format!("unsupported duration {text}"));
        }
        let amount = rest[..digits]
            .parse::<u64>()
            .map_err(|_| format!("duration {text} is too large"))?;
        rest = &rest[digits..];
        let (unit, scale) = [
            ("ms", 1),
            ("s", 1000),
            ("m", 60_000),
            ("h", 3_600_000),
            ("d", 86_400_000),
            ("w", 604_800_000),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))
        .ok_or_else(|| format!("unsupported duration {text}"))?;
        total = total
            .checked_add(amount.checked_mul(scale).ok_or("duration overflow")?)
            .ok_or("duration overflow")?;
        rest = &rest[unit.len()..];
    }
    if total == 0 {
        return Err(format!("duration {text} must be positive"));
    }
    Ok(total)
}

pub fn duration_text(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

fn escaped(text: &str, regex: bool) -> String {
    let mut out = String::new();
    for character in text.chars() {
        if regex && "\\.^$|?*+()[]{}".contains(character) {
            out.push_str("\\\\");
        }
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character => out.push(character),
        }
    }
    out
}

/// Only complete double-quoted label values and duration tokens are expanded.
/// A raw or formatted interpolation is refused rather than changing its scope.
pub fn interpolate(
    target: &PanelQuery,
    vars: &Variables,
    scrape_ms: u64,
) -> Result<(String, u64), String> {
    let min_step = if target.min_step.is_empty() {
        scrape_ms
    } else {
        duration_ms(&target.min_step)?
    };
    let panel_min = if vars.min_interval.is_empty() {
        0
    } else {
        duration_ms(&vars.min_interval)?
    };
    let interval = vars.step_ms.max(min_step).max(panel_min);
    let rate = interval
        .checked_add(min_step)
        .ok_or("rate interval overflow")?
        .max(min_step.checked_mul(4).ok_or("rate interval overflow")?);
    if vars.end_ms < vars.start_ms || interval == 0 {
        return Err("invalid panel time range".to_string());
    }
    let expression = target.expr.as_str();
    let mut output = String::with_capacity(expression.len());
    let mut cursor = 0;
    while let Some(offset) = expression[cursor..].find('$') {
        let at = cursor + offset;
        output.push_str(&expression[cursor..at]);
        let rest = &expression[at + 1..];
        let (name, consumed) = if let Some(braced) = rest.strip_prefix('{') {
            let end = braced.find('}').ok_or("unterminated Grafana variable")?;
            (&braced[..end], end + 2)
        } else {
            let len = rest
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                .count();
            (&rest[..len], len)
        };
        if name.is_empty()
            || name
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_digit())
        {
            output.push('$');
            cursor = at + 1;
            continue;
        }
        let after = at + 1 + consumed;
        let replacement = match name {
            "__interval" => duration_text(interval),
            "__range" => duration_text((vars.end_ms - vars.start_ms) as u64),
            "__rate_interval" => duration_text(rate),
            "namespace" | "pod" | "node" => {
                let prefix = expression[..at].trim_end();
                if !prefix.ends_with('"') || !expression[after..].starts_with('"') {
                    return Err(format!(
                        "${name} must be a complete double-quoted label value"
                    ));
                }
                let matcher = prefix[..prefix.len() - 1].trim_end();
                let regex = matcher.ends_with("=~") || matcher.ends_with("!~");
                if !regex && !matcher.ends_with('=') {
                    return Err(format!("${name} is outside a label matcher"));
                }
                match name {
                    "namespace" if !vars.namespace.is_empty() => escaped(&vars.namespace, regex),
                    "node" => escaped(
                        vars.node
                            .as_deref()
                            .filter(|node| !node.is_empty())
                            .ok_or("$node needs a selected pod's node identity")?,
                        regex,
                    ),
                    "pod" if vars.pods.len() == 1 => escaped(&vars.pods[0], regex),
                    "pod" if regex && !vars.pods.is_empty() => format!(
                        "(?:{})",
                        vars.pods
                            .iter()
                            .map(|pod| escaped(pod, true))
                            .collect::<Vec<_>>()
                            .join("|")
                    ),
                    "pod" => {
                        return Err(
                            "$pod requires pods and a regex matcher for multiple pods".to_string()
                        );
                    }
                    _ => return Err(format!("${name} has no selected value")),
                }
            }
            _ => return Err(format!("unsupported Grafana variable ${name}")),
        };
        output.push_str(&replacement);
        cursor = after;
    }
    output.push_str(&expression[cursor..]);
    if output.len() > super::super::prom::MAX_EXPR_BYTES {
        return Err("expanded panel query exceeds 8 KiB".to_string());
    }
    Ok((output, interval))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(expr: &str) -> PanelQuery {
        PanelQuery {
            ref_id: "A".into(),
            expr: expr.into(),
            dialect: super::super::QueryDialect::PromQL,
            datasource: Some("prometheus".into()),
            min_step: String::new(),
            unsupported: None,
        }
    }
    fn vars() -> Variables {
        Variables {
            namespace: "pilot".into(),
            pods: vec!["api.0".into()],
            node: Some("node-0".into()),
            start_ms: 0,
            end_ms: 900_000,
            step_ms: 10_000,
            min_interval: String::new(),
        }
    }
    #[test]
    fn labels_and_all_duration_macros_follow_scrape_and_min_step() {
        let target = target(
            r#"rate(x{namespace="$namespace",pod=~"$pod",node="$node"}[$__rate_interval]) + y[$__interval] + z[$__range]"#,
        );
        let (expr, step) = interpolate(&target, &vars(), 15_000).unwrap();
        assert_eq!(step, 15_000);
        assert_eq!(
            expr,
            r#"rate(x{namespace="pilot",pod=~"api\\.0",node="node-0"}[60s]) + y[15s] + z[900s]"#
        );
        let mut target = target;
        target.min_step = "30s".into();
        let mut vars = vars();
        vars.min_interval = "2m".into();
        let (expr, step) = interpolate(&target, &vars, 15_000).unwrap();
        assert_eq!(step, 120_000);
        assert!(expr.contains("[150s]"));
    }
    #[test]
    fn injection_multiselect_and_unknown_variables_are_explicit() {
        let mut vars = vars();
        vars.namespace = "a\"} or x{n=\"b".into();
        let (expr, _) =
            interpolate(&target(r#"x{namespace="$namespace"}"#), &vars, 15_000).unwrap();
        assert_eq!(expr, r#"x{namespace="a\"} or x{n=\"b"}"#);
        vars.pods.push("api-01".into());
        assert!(interpolate(&target(r#"x{pod="$pod"}"#), &vars, 15_000).is_err());
        for expr in [
            "x[$missing]",
            "x[${pod:raw}]",
            "x[$pod]",
            r#"x{pod=~"$pod.*"}"#,
        ] {
            assert!(interpolate(&target(expr), &vars, 15_000).is_err(), "{expr}");
        }
    }
}
