//! Filters govulncheck NDJSON output. Deduplicates findings by OSV id and
//! shows a compact summary with module@version, fix, and call-site location.

use crate::core::runner;
use crate::core::utils::{resolved_command, truncate};
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;

const MAX_FINDINGS: usize = 20;

#[derive(Debug, Deserialize, Clone)]
struct Position {
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    line: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
struct TraceFrame {
    #[serde(default)]
    module: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    package: Option<String>,
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    position: Option<Position>,
}

#[derive(Debug, Deserialize, Clone)]
struct Finding {
    osv: String,
    #[serde(default)]
    fixed_version: Option<String>,
    #[serde(default)]
    trace: Vec<TraceFrame>,
}

#[derive(Debug, Deserialize)]
struct OsvEntry {
    id: String,
    #[serde(default)]
    summary: Option<String>,
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let (stripped, full) = strip_full_flag(args);
    let final_args = inject_json_flag(&stripped);

    let mut cmd = resolved_command("govulncheck");
    for arg in &final_args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: govulncheck {}", final_args.join(" "));
    }

    let filter: Box<dyn Fn(&str) -> String> = if full {
        Box::new(|s: &str| s.to_string())
    } else {
        Box::new(filter_govulncheck_json)
    };

    let exit_code = runner::run_filtered(
        cmd,
        "govulncheck",
        &args.join(" "),
        |s: &str| filter(s),
        runner::RunOptions::stdout_only().tee("govulncheck"),
    )?;

    // govulncheck: exit 0 = clean, exit 3 = vulns found, others = tool error.
    // Pass through as-is so callers see the canonical status.
    Ok(exit_code)
}

/// Inject `-json` if not already present in args.
fn inject_json_flag(args: &[String]) -> Vec<String> {
    let has_json = args.iter().any(|a| a == "-json" || a == "--json");

    let mut out = Vec::with_capacity(args.len() + 1);
    if !has_json {
        out.push("-json".to_string());
    }
    out.extend_from_slice(args);
    out
}

/// Strip a private `--full` flag and return (remaining_args, full_requested).
fn strip_full_flag(args: &[String]) -> (Vec<String>, bool) {
    let mut full = false;
    let remaining: Vec<String> = args
        .iter()
        .filter(|a| {
            if a.as_str() == "--full" {
                full = true;
                false
            } else {
                true
            }
        })
        .cloned()
        .collect();
    (remaining, full)
}

/// Aggregated per-OSV finding info.
#[derive(Default)]
struct VulnSummary {
    summary: Option<String>,
    fixed_version: Option<String>,
    /// Set of (module, version) pairs affected.
    affected: Vec<(String, String)>,
    /// First trace frame with usable position info, used for the call site line.
    call_site: Option<(String, u64, String)>, // (file, line, function)
}

/// Filter govulncheck NDJSON output. Each line is a tagged-union JSON object;
/// we care about `osv` (vulnerability metadata) and `finding` (call traces).
pub(crate) fn filter_govulncheck_json(input: &str) -> String {
    // Preserve insertion order (first-seen) by OSV id.
    let mut order: Vec<String> = Vec::new();
    let mut entries: BTreeMap<String, VulnSummary> = BTreeMap::new();

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let obj = match value.as_object() {
            Some(o) => o,
            None => continue,
        };

        if let Some(osv_value) = obj.get("osv") {
            if let Ok(osv) = serde_json::from_value::<OsvEntry>(osv_value.clone()) {
                let entry = entries.entry(osv.id.clone()).or_default();
                if entry.summary.is_none() {
                    entry.summary = osv.summary;
                }
                if !order.contains(&osv.id) {
                    order.push(osv.id);
                }
            }
            continue;
        }

        if let Some(finding_value) = obj.get("finding") {
            if let Ok(finding) = serde_json::from_value::<Finding>(finding_value.clone()) {
                if !order.contains(&finding.osv) {
                    order.push(finding.osv.clone());
                }
                let entry = entries.entry(finding.osv.clone()).or_default();
                if entry.fixed_version.is_none() {
                    entry.fixed_version = finding.fixed_version.clone();
                }

                // Collect affected (module, version) — usually first frame in trace
                // is the vulnerable module.
                if let Some(frame) = finding.trace.first() {
                    let module = frame.module.clone().unwrap_or_default();
                    let version = frame.version.clone().unwrap_or_default();
                    if !module.is_empty() {
                        let pair = (module, version);
                        if !entry.affected.contains(&pair) {
                            entry.affected.push(pair);
                        }
                    }
                }

                // Call site: first frame with position+filename+line.
                if entry.call_site.is_none() {
                    for frame in &finding.trace {
                        if let Some(pos) = &frame.position {
                            if let (Some(file), Some(line)) = (&pos.filename, pos.line) {
                                let func = frame.function.clone().unwrap_or_default();
                                entry.call_site = Some((file.clone(), line, func));
                                break;
                            }
                        }
                    }
                }
            }
            continue;
        }
        // Other tags (config, progress) are ignored.
    }

    // Only keep OSVs that have at least one finding (i.e. a real vulnerability,
    // not just db metadata). We detect this via affected being non-empty OR
    // fixed_version being present OR call_site being present.
    let real: Vec<&String> = order
        .iter()
        .filter(|id| {
            entries
                .get(*id)
                .map(|e| !e.affected.is_empty() || e.fixed_version.is_some() || e.call_site.is_some())
                .unwrap_or(false)
        })
        .collect();

    if real.is_empty() {
        return "govulncheck: clean (no vulnerabilities found)".to_string();
    }

    let total = real.len();
    let mut result = String::new();
    result.push_str(&format!(
        "govulncheck: {} finding{}\n",
        total,
        if total == 1 { "" } else { "s" },
    ));
    result.push_str("═══════════════════════════════════════\n");

    for id in real.iter().take(MAX_FINDINGS) {
        let entry = match entries.get(*id) {
            Some(e) => e,
            None => continue,
        };

        // Build affected string: mod@v1,mod2@v2
        let affected = if entry.affected.is_empty() {
            String::new()
        } else {
            entry
                .affected
                .iter()
                .map(|(m, v)| {
                    if v.is_empty() {
                        m.clone()
                    } else {
                        format!("{}@{}", m, v)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };

        let fix = entry
            .fixed_version
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|f| format!(" → {}", f))
            .unwrap_or_default();

        let head = if affected.is_empty() {
            format!("  {}{}", id, fix)
        } else {
            format!("  {}  {}{}", id, affected, fix)
        };
        result.push_str(&format!("{}\n", truncate(&head, 160)));

        if let Some((file, line, func)) = &entry.call_site {
            let func_part = if func.is_empty() {
                String::new()
            } else {
                format!("  {}", func)
            };
            result.push_str(&format!(
                "    {}:{}{}\n",
                file,
                line,
                truncate(&func_part, 100),
            ));
        }
    }

    if total > MAX_FINDINGS {
        result.push_str(&format!("\n… +{} more findings\n", total - MAX_FINDINGS));
        if let Some(hint) = crate::core::tee::force_tee_tail_hint(input, "govulncheck", 1) {
            result.push_str(&format!("  {}\n", hint));
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_clean() {
        // Only config + progress events, no osv/finding.
        let input = r#"{"config":{"protocol_version":"v1.0.0","scanner_name":"govulncheck","scanner_version":"v1.0.0","db":"https://vuln.go.dev"}}
{"progress":{"message":"Scanning your code and 42 packages..."}}"#;
        let result = filter_govulncheck_json(input);
        assert_eq!(result, "govulncheck: clean (no vulnerabilities found)");
    }

    #[test]
    fn test_filter_clean_empty() {
        assert_eq!(
            filter_govulncheck_json(""),
            "govulncheck: clean (no vulnerabilities found)"
        );
    }

    #[test]
    fn test_filter_single_finding() {
        let input = r#"{"config":{"protocol_version":"v1.0.0"}}
{"progress":{"message":"Scanning..."}}
{"osv":{"id":"GO-2023-1234","summary":"buffer overflow in foo"}}
{"finding":{"osv":"GO-2023-1234","fixed_version":"v1.2.4","trace":[{"module":"foo","version":"v1.2.3","package":"foo","function":"Bar","position":{"filename":"main.go","line":42,"column":5}}]}}"#;
        let result = filter_govulncheck_json(input);
        assert!(result.contains("govulncheck: 1 finding"), "got: {}", result);
        assert!(result.contains("GO-2023-1234"), "got: {}", result);
        assert!(result.contains("foo@v1.2.3"), "got: {}", result);
        assert!(result.contains("v1.2.4"), "got: {}", result);
        assert!(result.contains("main.go:42"), "got: {}", result);
        assert!(result.contains("Bar"), "got: {}", result);
    }

    #[test]
    fn test_filter_multiple_findings_dedupe() {
        // Two findings reference the same OSV — should be merged into one entry.
        let input = r#"{"osv":{"id":"GO-2023-1234","summary":"vuln"}}
{"finding":{"osv":"GO-2023-1234","fixed_version":"v1.2.4","trace":[{"module":"foo","version":"v1.2.3","function":"Bar","position":{"filename":"a.go","line":10}}]}}
{"finding":{"osv":"GO-2023-1234","fixed_version":"v1.2.4","trace":[{"module":"foo","version":"v1.2.3","function":"Baz","position":{"filename":"b.go","line":20}}]}}"#;
        let result = filter_govulncheck_json(input);
        assert!(result.contains("govulncheck: 1 finding"), "got: {}", result);
        // Only one occurrence of the OSV id in the body.
        assert_eq!(
            result.matches("GO-2023-1234").count(),
            1,
            "got: {}",
            result
        );
    }

    #[test]
    fn test_filter_corrupted_json_skipped() {
        let input = r#"{"osv":{"id":"GO-2023-1234"}}
not valid json
{this is broken
{"finding":{"osv":"GO-2023-1234","fixed_version":"v1.2.4","trace":[{"module":"foo","version":"v1.2.3","function":"Bar","position":{"filename":"main.go","line":42}}]}}"#;
        let result = filter_govulncheck_json(input);
        assert!(result.contains("1 finding"), "got: {}", result);
        assert!(result.contains("GO-2023-1234"), "got: {}", result);
    }

    #[test]
    fn test_filter_truncate() {
        // 25 distinct OSV findings → truncated to 20 + tee hint line.
        let mut lines: Vec<String> = Vec::new();
        for i in 1..=25 {
            lines.push(format!(
                r#"{{"osv":{{"id":"GO-2023-{i:04}","summary":"vuln{i}"}}}}"#
            ));
            lines.push(format!(
                r#"{{"finding":{{"osv":"GO-2023-{i:04}","fixed_version":"v1.0.{i}","trace":[{{"module":"m{i}","version":"v0.1.0","function":"F{i}","position":{{"filename":"f{i}.go","line":{i}}}}}]}}}}"#
            ));
        }
        let input = lines.join("\n");
        let result = filter_govulncheck_json(&input);
        assert!(
            result.contains("govulncheck: 25 findings"),
            "got: {}",
            result
        );
        assert!(result.contains("+5 more findings"), "got: {}", result);
        // First 20 must be present, 21st must NOT be in the body listing.
        assert!(result.contains("GO-2023-0001"), "got: {}", result);
        assert!(result.contains("GO-2023-0020"), "got: {}", result);
        assert!(!result.contains("GO-2023-0021"), "got: {}", result);
    }

    #[test]
    fn test_inject_json_flag_when_missing() {
        let args = vec!["./...".to_string()];
        let result = inject_json_flag(&args);
        assert_eq!(result, vec!["-json", "./..."]);
    }

    #[test]
    fn test_inject_json_flag_idempotent() {
        let args = vec!["-json".to_string(), "./...".to_string()];
        let result = inject_json_flag(&args);
        assert_eq!(result, args);
    }

    #[test]
    fn test_strip_full_flag_present() {
        let args = vec!["--full".to_string(), "./...".to_string()];
        let (rest, full) = strip_full_flag(&args);
        assert_eq!(rest, vec!["./...".to_string()]);
        assert!(full);
    }

    #[test]
    fn test_strip_full_flag_absent() {
        let args = vec!["./...".to_string()];
        let (rest, full) = strip_full_flag(&args);
        assert_eq!(rest, vec!["./...".to_string()]);
        assert!(!full);
    }
}
