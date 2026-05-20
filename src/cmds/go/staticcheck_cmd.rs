//! Filters staticcheck NDJSON output, clustering by rule code.

use crate::core::runner;
use crate::core::utils::{resolved_command, truncate};
use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;

const MAX_RULES: usize = 5;
const MAX_SAMPLES_PER_RULE: usize = 3;
const TEE_HINT_THRESHOLD: usize = 50;

#[derive(Debug, Deserialize)]
struct Location {
    file: String,
    line: u64,
    #[serde(default)]
    #[allow(dead_code)]
    column: u64,
}

#[derive(Debug, Deserialize)]
struct Issue {
    code: String,
    #[serde(default)]
    #[allow(dead_code)]
    severity: String,
    location: Location,
    message: String,
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let final_args = inject_format_flag(args);

    let mut cmd = resolved_command("staticcheck");
    for arg in &final_args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: staticcheck {}", final_args.join(" "));
    }

    let exit_code = runner::run_filtered(
        cmd,
        "staticcheck",
        &args.join(" "),
        filter_staticcheck_json,
        runner::RunOptions::stdout_only().tee("staticcheck"),
    )?;

    // staticcheck: exit 0 = clean, exit 1 = issues found (not an error),
    // exit 2+ = config/build error. Treat lint findings as success so the hook
    // does not interrupt the LLM (mirrors golangci_cmd behaviour).
    Ok(if exit_code == 1 { 0 } else { exit_code })
}

/// Inject `-f json` if the user did not already specify a format.
/// `staticcheck` uses `-f` / `-format` (single dash), not `--format`.
fn inject_format_flag(args: &[String]) -> Vec<String> {
    let has_format = args.iter().any(|a| {
        a == "-f"
            || a == "-format"
            || a.starts_with("-f=")
            || a.starts_with("-format=")
    });

    let mut out = Vec::with_capacity(args.len() + 2);
    if !has_format {
        out.push("-f".to_string());
        out.push("json".to_string());
    }
    out.extend_from_slice(args);
    out
}

/// Filter staticcheck NDJSON output. Clusters issues by `code`, picks the top
/// rules by count, and shows the first few samples per rule.
pub(crate) fn filter_staticcheck_json(input: &str) -> String {
    let mut issues: Vec<Issue> = Vec::new();

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        if let Ok(issue) = serde_json::from_str::<Issue>(trimmed) {
            issues.push(issue);
        }
    }

    if issues.is_empty() {
        return "staticcheck: clean (0 issues)".to_string();
    }

    let total = issues.len();

    // Cluster by rule code.
    let mut by_code: HashMap<String, Vec<&Issue>> = HashMap::new();
    for issue in &issues {
        by_code.entry(issue.code.clone()).or_default().push(issue);
    }

    let mut clusters: Vec<(String, Vec<&Issue>)> = by_code.into_iter().collect();
    // Sort by count desc, then by code asc for determinism on ties.
    clusters.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    let rule_count = clusters.len();

    let mut result = String::new();
    result.push_str(&format!(
        "staticcheck: {} issue{} across {} rule{}\n",
        total,
        if total == 1 { "" } else { "s" },
        rule_count,
        if rule_count == 1 { "" } else { "s" },
    ));
    result.push_str("═══════════════════════════════════════\n");

    for (code, rule_issues) in clusters.iter().take(MAX_RULES) {
        let count = rule_issues.len();
        // Use the first issue's message as the rule headline.
        let headline = rule_issues
            .first()
            .map(|i| i.message.as_str())
            .unwrap_or("");
        result.push_str(&format!(
            "{} ({}): {}\n",
            code,
            count,
            truncate(headline, 120),
        ));

        for issue in rule_issues.iter().take(MAX_SAMPLES_PER_RULE) {
            result.push_str(&format!(
                "  {}:{}  {}\n",
                issue.location.file,
                issue.location.line,
                truncate(&issue.message, 120),
            ));
        }

        if count > MAX_SAMPLES_PER_RULE {
            result.push_str(&format!(
                "  ... +{} more\n",
                count - MAX_SAMPLES_PER_RULE,
            ));
        }
    }

    if rule_count > MAX_RULES {
        result.push_str(&format!("\n... +{} more rules\n", rule_count - MAX_RULES));
    }

    if total > TEE_HINT_THRESHOLD {
        if let Some(hint) =
            crate::core::tee::force_tee_tail_hint(input, "staticcheck", 1)
        {
            result.push_str(&format!("\n{}\n", hint));
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_clean() {
        let result = filter_staticcheck_json("");
        assert_eq!(result, "staticcheck: clean (0 issues)");
    }

    #[test]
    fn test_filter_clean_with_blank_and_noise_lines() {
        // Blank lines and non-JSON noise should be skipped without affecting "clean".
        let input = "\n\nnot json garbage\n  \n";
        let result = filter_staticcheck_json(input);
        assert_eq!(result, "staticcheck: clean (0 issues)");
    }

    #[test]
    fn test_filter_single_issue() {
        let input = r#"{"code":"SA1000","severity":"error","location":{"file":"a.go","line":10,"column":2},"end":{"file":"a.go","line":10,"column":20},"message":"invalid regular expression: missing closing )"}"#;
        let result = filter_staticcheck_json(input);
        assert!(result.contains("1 issue across 1 rule"), "got: {}", result);
        assert!(result.contains("SA1000 (1):"));
        assert!(result.contains("a.go:10"));
        assert!(result.contains("invalid regular expression"));
    }

    #[test]
    fn test_filter_clustering() {
        // 5 SA1000 + 2 SA4006 → 7 issues, 2 rules, SA1000 listed first (higher count).
        let mut lines: Vec<String> = Vec::new();
        for i in 1..=5 {
            lines.push(format!(
                r#"{{"code":"SA1000","severity":"error","location":{{"file":"a{i}.go","line":{i},"column":1}},"message":"invalid regular expression"}}"#
            ));
        }
        for i in 1..=2 {
            lines.push(format!(
                r#"{{"code":"SA4006","severity":"warning","location":{{"file":"b{i}.go","line":{i},"column":1}},"message":"this value is never used"}}"#
            ));
        }
        let input = lines.join("\n");
        let result = filter_staticcheck_json(&input);

        assert!(result.contains("7 issues across 2 rules"), "got: {}", result);
        let sa1000_pos = result.find("SA1000").expect("SA1000 missing");
        let sa4006_pos = result.find("SA4006").expect("SA4006 missing");
        assert!(
            sa1000_pos < sa4006_pos,
            "SA1000 (higher count) should be listed first"
        );
        assert!(result.contains("SA1000 (5):"));
        assert!(result.contains("SA4006 (2):"));
    }

    #[test]
    fn test_filter_truncate_per_rule() {
        // 10 issues for one rule → 3 samples + "+7 more"
        let mut lines: Vec<String> = Vec::new();
        for i in 1..=10 {
            lines.push(format!(
                r#"{{"code":"SA1000","severity":"error","location":{{"file":"a{i}.go","line":{i},"column":1}},"message":"invalid regular expression"}}"#
            ));
        }
        let input = lines.join("\n");
        let result = filter_staticcheck_json(&input);

        assert!(result.contains("SA1000 (10):"));
        assert!(result.contains("... +7 more"), "got: {}", result);
        // Should show exactly 3 sample lines (count occurrences of file pattern).
        let sample_count = result.matches(".go:").count();
        assert_eq!(sample_count, 3, "expected 3 samples, got: {}", result);
    }

    #[test]
    fn test_filter_truncate_total() {
        // >50 issues → force_tee_tail_hint may produce output if tee is enabled.
        // The function must never panic and must include the issue summary.
        let mut lines: Vec<String> = Vec::new();
        for i in 1..=60 {
            lines.push(format!(
                r#"{{"code":"SA1000","severity":"error","location":{{"file":"a{i}.go","line":{i},"column":1}},"message":"invalid regular expression"}}"#
            ));
        }
        let input = lines.join("\n");
        let result = filter_staticcheck_json(&input);

        assert!(
            result.contains("60 issues across 1 rule"),
            "got: {}",
            result
        );
        assert!(result.contains("SA1000 (60):"));
        // Hint may or may not be present depending on tee config; do not assert
        // its presence here. The threshold branch is exercised by coverage.
    }

    #[test]
    fn test_filter_corrupted_skipped() {
        // Mix of bad JSON and good objects — bad lines are ignored.
        let input = r#"not a json line
{"code":"SA1000","severity":"error","location":{"file":"a.go","line":10,"column":2},"message":"invalid regular expression"}
{this is also broken
{"code":"SA4006","severity":"warning","location":{"file":"b.go","line":5,"column":10},"message":"this value is never used"}
"#;
        let result = filter_staticcheck_json(input);
        assert!(result.contains("2 issues across 2 rules"), "got: {}", result);
        assert!(result.contains("a.go:10"));
        assert!(result.contains("b.go:5"));
    }

    #[test]
    fn test_filter_top_5_rules_truncates_extras() {
        // 6 distinct rule codes — only top 5 shown, extras roll up to "+1 more rules".
        let lines: Vec<String> = (1..=6)
            .map(|i| {
                format!(
                    r#"{{"code":"SA{:04}","severity":"error","location":{{"file":"f{i}.go","line":{i},"column":1}},"message":"msg{i}"}}"#,
                    1000 + i
                )
            })
            .collect();
        let input = lines.join("\n");
        let result = filter_staticcheck_json(&input);
        assert!(result.contains("6 issues across 6 rules"), "got: {}", result);
        assert!(result.contains("... +1 more rules"), "got: {}", result);
    }

    #[test]
    fn test_inject_format_flag_when_missing() {
        let args = vec!["./...".to_string()];
        let result = inject_format_flag(&args);
        assert_eq!(result, vec!["-f", "json", "./..."]);
    }

    #[test]
    fn test_inject_format_flag_idempotent_with_separate_value() {
        let args = vec!["-f".to_string(), "json".to_string(), "./...".to_string()];
        let result = inject_format_flag(&args);
        assert_eq!(result, args);
    }

    #[test]
    fn test_inject_format_flag_respects_user_choice() {
        // User chose `-f stylish` — RTK must not overwrite it.
        let args = vec!["-f".to_string(), "stylish".to_string()];
        let result = inject_format_flag(&args);
        assert_eq!(result, args);
    }

    #[test]
    fn test_inject_format_flag_idempotent_with_inline_value() {
        let args = vec!["-f=json".to_string(), "./...".to_string()];
        let result = inject_format_flag(&args);
        assert_eq!(result, args);
    }

    #[test]
    fn test_inject_format_flag_recognizes_long_form() {
        let args = vec!["-format".to_string(), "json".to_string()];
        let result = inject_format_flag(&args);
        assert_eq!(result, args);

        let args2 = vec!["-format=stylish".to_string()];
        let result2 = inject_format_flag(&args2);
        assert_eq!(result2, args2);
    }
}
