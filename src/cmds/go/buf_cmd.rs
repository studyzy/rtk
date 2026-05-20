//! Filters buf (Protobuf) output — lint (NDJSON cluster), build, generate.

use crate::core::runner;
use crate::core::tracking;
use crate::core::utils::{exit_code_from_output, resolved_command, truncate};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

/// FileAnnotation — single buf lint NDJSON record.
#[derive(Debug, Deserialize)]
struct BufLintAnnotation {
    path: Option<String>,
    start_line: Option<u32>,
    start_column: Option<u32>,
    #[serde(rename = "type")]
    rule_type: Option<String>,
    message: Option<String>,
}

/// Single clustered lint issue.
#[derive(Debug, Clone)]
struct LintIssue {
    path: String,
    line: u32,
    col: u32,
    rule: String,
    message: String,
}

const MAX_RULES_DISPLAYED: usize = 5;
const MAX_SAMPLES_PER_RULE: usize = 3;
const MAX_LINE_WIDTH: usize = 120;

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    if args.is_empty() {
        anyhow::bail!("buf: no subcommand specified");
    }

    match args[0].as_str() {
        "lint" => run_lint(&args[1..], verbose),
        "build" => run_build(&args[1..], verbose),
        "generate" => run_generate(&args[1..], verbose),
        _ => run_other(args, verbose),
    }
}

fn run_lint(args: &[String], verbose: u8) -> Result<i32> {
    let injected = inject_lint_format(args);

    let mut cmd = resolved_command("buf");
    cmd.arg("lint");
    for arg in &injected {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: buf lint {}", injected.join(" "));
    }

    runner::run_filtered(
        cmd,
        "buf lint",
        &injected.join(" "),
        filter_buf_lint_json,
        runner::RunOptions::with_tee("buf_lint"),
    )
}

fn run_build(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("buf");
    cmd.arg("build");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: buf build {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "buf build",
        &args.join(" "),
        filter_buf_build,
        runner::RunOptions::with_tee("buf_build"),
    )
}

fn run_generate(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("buf");
    cmd.arg("generate");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: buf generate {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "buf generate",
        &args.join(" "),
        filter_buf_generate,
        runner::RunOptions::with_tee("buf_generate"),
    )
}

/// Passthrough for non-lint/build/generate subcommands (format/breaking/mod/push/registry, etc).
fn run_other(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    let subcommand = args[0].clone();
    let mut cmd = resolved_command("buf");
    cmd.arg(&subcommand);
    for arg in &args[1..] {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: buf {} ...", subcommand);
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run buf {}", subcommand))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    print!("{}", stdout);
    eprint!("{}", stderr);

    timer.track(
        &format!("buf {}", subcommand),
        &format!("rtk buf {}", subcommand),
        &raw,
        &raw,
    );

    Ok(exit_code_from_output(&output, "buf"))
}

/// Inject `--error-format=json` (kept as-is when user already specified `--error-format`).
fn inject_lint_format(args: &[String]) -> Vec<String> {
    let already_set = args.iter().any(|a| {
        a == "--error-format" || a.starts_with("--error-format=")
    });

    if already_set {
        args.to_vec()
    } else {
        let mut v = Vec::with_capacity(args.len() + 1);
        v.push("--error-format=json".to_string());
        v.extend(args.iter().cloned());
        v
    }
}

/// Filter buf lint NDJSON output, clustering by rule code.
pub(crate) fn filter_buf_lint_json(input: &str) -> String {
    let mut issues: Vec<LintIssue> = Vec::new();

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }

        let ann: BufLintAnnotation = match serde_json::from_str(trimmed) {
            Ok(a) => a,
            Err(_) => continue, // tolerate malformed / partial JSON lines
        };

        // Require path/type/message; otherwise drop the record.
        let path = match ann.path {
            Some(p) if !p.is_empty() => p,
            _ => continue,
        };
        let rule = ann.rule_type.unwrap_or_else(|| "UNKNOWN".to_string());
        let message = ann.message.unwrap_or_default();

        issues.push(LintIssue {
            path,
            line: ann.start_line.unwrap_or(0),
            col: ann.start_column.unwrap_or(0),
            rule,
            message,
        });
    }

    if issues.is_empty() {
        return "buf lint: clean (0 issues)".to_string();
    }

    // Cluster by rule code.
    let mut by_rule: HashMap<String, Vec<LintIssue>> = HashMap::new();
    for issue in &issues {
        by_rule
            .entry(issue.rule.clone())
            .or_default()
            .push(issue.clone());
    }

    let total_issues = issues.len();
    let total_rules = by_rule.len();

    let mut rule_entries: Vec<(String, Vec<LintIssue>)> = by_rule.into_iter().collect();
    // Descending by count, ties broken by rule name (stable output).
    rule_entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    let mut result = String::new();
    result.push_str(&format!(
        "buf lint: {} issues across {} rules\n",
        total_issues, total_rules
    ));
    result.push_str("═══════════════════════════════════════\n");

    for (rule, rule_issues) in rule_entries.iter().take(MAX_RULES_DISPLAYED) {
        // Header description uses the first message under the rule.
        let header_msg = rule_issues
            .first()
            .map(|i| i.message.as_str())
            .unwrap_or("");
        result.push_str(&format!(
            "{} ({}): {}\n",
            rule,
            rule_issues.len(),
            header_msg
        ));

        for issue in rule_issues.iter().take(MAX_SAMPLES_PER_RULE) {
            let location = format!("{}:{}:{}", issue.path, issue.line, issue.col);
            let line = format!("  {}  {}", location, issue.message);
            result.push_str(&truncate(&line, MAX_LINE_WIDTH));
            result.push('\n');
        }

        if rule_issues.len() > MAX_SAMPLES_PER_RULE {
            result.push_str(&format!(
                "  ... +{} more\n",
                rule_issues.len() - MAX_SAMPLES_PER_RULE
            ));
        }
    }

    if rule_entries.len() > MAX_RULES_DISPLAYED {
        result.push_str(&format!(
            "\n... +{} more rules\n",
            rule_entries.len() - MAX_RULES_DISPLAYED
        ));
    }

    result.trim().to_string()
}

/// Filter buf build output — suppress success, keep error lines.
pub(crate) fn filter_buf_build(input: &str) -> String {
    let mut errors: Vec<String> = Vec::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip pure progress/status lines (buf build typically writes errors to stderr).
        if is_buf_progress_line(trimmed) {
            continue;
        }
        errors.push(trimmed.to_string());
    }

    if errors.is_empty() {
        return "buf build: ok".to_string();
    }

    let mut result = String::new();
    result.push_str(&format!("buf build: {} errors\n", errors.len()));
    result.push_str("═══════════════════════════════════════\n");
    for err in &errors {
        result.push_str(&truncate(err, MAX_LINE_WIDTH));
        result.push('\n');
    }
    result.trim().to_string()
}

/// Filter buf generate output — emit ok on full success, keep failed plugin output verbatim.
pub(crate) fn filter_buf_generate(input: &str) -> String {
    // buf generate is mostly silent on success; failures surface plugin errors marked by
    // "✗", "error:", "Failure: " or "plugin <name>: ...". Heuristic: count plugin
    // invocations and failures, keep any line containing error/✗/failure/exit.

    let mut plugin_count: usize = 0;
    let mut failed_plugins: Vec<(String, Vec<String>)> = Vec::new();
    let mut current_failure: Option<(String, Vec<String>)> = None;
    let mut bare_error_lines: Vec<String> = Vec::new();

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_buf_progress_line(trimmed) {
            continue;
        }

        // Count plugin invocation lines (typical: "Generating <plugin>..." / "plugin <name>").
        if let Some(name) = parse_plugin_invocation(trimmed) {
            // Archive previous unfinished failure first.
            if let Some(f) = current_failure.take() {
                failed_plugins.push(f);
            }
            plugin_count += 1;
            // Default: not a failure.
            let _ = name; // count only
            continue;
        }

        // Failure-start marker.
        if let Some(failed_name) = parse_plugin_failure_marker(trimmed) {
            // Archive previous failure.
            if let Some(f) = current_failure.take() {
                failed_plugins.push(f);
            }
            current_failure = Some((failed_name, Vec::new()));
            continue;
        }

        // Error/exception line.
        if is_buf_error_line(trimmed) {
            if let Some((_, ref mut buf)) = current_failure {
                buf.push(trimmed.to_string());
            } else {
                bare_error_lines.push(trimmed.to_string());
            }
            continue;
        }

        // Currently collecting a failed plugin's context lines — keep accumulating.
        if let Some((_, ref mut buf)) = current_failure {
            buf.push(trimmed.to_string());
        }
    }

    if let Some(f) = current_failure.take() {
        failed_plugins.push(f);
    }

    let total_failed = failed_plugins.len()
        + if !bare_error_lines.is_empty() && failed_plugins.is_empty() {
            1
        } else {
            0
        };

    if total_failed == 0 && bare_error_lines.is_empty() {
        if plugin_count > 0 {
            return format!("buf generate: ok ({} plugins)", plugin_count);
        }
        return "buf generate: ok".to_string();
    }

    let mut result = String::new();
    let plugins_label = plugin_count.max(failed_plugins.len() + 1); // include the failed one at minimum
    let displayed_failed = if failed_plugins.is_empty() && !bare_error_lines.is_empty() {
        1
    } else {
        failed_plugins.len()
    };
    result.push_str(&format!(
        "buf generate: {} plugins, {} failed\n",
        plugins_label, displayed_failed
    ));

    for (name, lines) in &failed_plugins {
        let first = lines
            .iter()
            .find(|l| !l.is_empty())
            .map(|s| s.as_str())
            .unwrap_or("(no detail)");
        let line = format!("  ✗ {}: {}", name, first);
        result.push_str(&truncate(&line, MAX_LINE_WIDTH));
        result.push('\n');
        // Append a couple of trailing context lines.
        for extra in lines.iter().skip(1).take(2) {
            let l = format!("    {}", extra);
            result.push_str(&truncate(&l, MAX_LINE_WIDTH));
            result.push('\n');
        }
    }

    if failed_plugins.is_empty() && !bare_error_lines.is_empty() {
        for err in bare_error_lines.iter().take(3) {
            let line = format!("  ✗ {}", err);
            result.push_str(&truncate(&line, MAX_LINE_WIDTH));
            result.push('\n');
        }
    }

    result.trim().to_string()
}

fn is_buf_progress_line(line: &str) -> bool {
    // buf's own progress / banner lines; conservatively only filter clearly noise lines.
    line.starts_with("INFO\t")
        || line == "ok"
        || line.starts_with("buf: downloading ")
}

fn parse_plugin_invocation(line: &str) -> Option<String> {
    // "Generating <plugin>..." / "Running plugin <plugin>..."
    if let Some(rest) = line.strip_prefix("Generating ") {
        let name = rest.trim_end_matches('.').trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    if let Some(rest) = line.strip_prefix("Running plugin ") {
        let name = rest.trim_end_matches('.').trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

fn parse_plugin_failure_marker(line: &str) -> Option<String> {
    // Forms:
    //   "Failure: plugin <name>: ..."
    //   "plugin <name> failed: ..."
    //   "✗ <name>: ..."
    if let Some(rest) = line.strip_prefix("✗ ") {
        if let Some((name, _)) = rest.split_once(':') {
            return Some(name.trim().to_string());
        }
        return Some(rest.trim().to_string());
    }
    if let Some(rest) = line.strip_prefix("Failure: plugin ") {
        if let Some((name, _)) = rest.split_once(':') {
            return Some(name.trim().to_string());
        }
    }
    if let Some(rest) = line.strip_prefix("plugin ") {
        if let Some((name, tail)) = rest.split_once(' ') {
            if tail.starts_with("failed") {
                return Some(name.trim().to_string());
            }
        }
    }
    None
}

fn is_buf_error_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.starts_with("error:")
        || lower.contains(" error:")
        || lower.starts_with("failure:")
        || lower.contains("failed")
        || lower.contains("exit status")
        || lower.contains("could not")
        || lower.contains("cannot ")
        || line.contains(".proto:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_lint_clean() {
        let result = filter_buf_lint_json("");
        assert_eq!(result, "buf lint: clean (0 issues)");

        let result_ws = filter_buf_lint_json("\n   \n");
        assert_eq!(result_ws, "buf lint: clean (0 issues)");
    }

    #[test]
    fn test_filter_lint_clustering() {
        let input = r#"{"path":"foo.proto","start_line":12,"start_column":3,"end_line":12,"end_column":20,"type":"FIELD_LOWER_SNAKE_CASE","message":"Field name should be lower_snake_case"}
{"path":"baz.proto","start_line":8,"start_column":5,"end_line":8,"end_column":12,"type":"FIELD_LOWER_SNAKE_CASE","message":"Field name should be lower_snake_case"}
{"path":"bar.proto","start_line":5,"start_column":1,"end_line":5,"end_column":10,"type":"PACKAGE_DEFINED","message":"Files must have a package defined"}
{"path":"qux.proto","start_line":1,"start_column":1,"end_line":1,"end_column":1,"type":"PACKAGE_DEFINED","message":"Files must have a package defined"}
{"path":"abc.proto","start_line":3,"start_column":1,"end_line":3,"end_column":5,"type":"ENUM_VALUE_PREFIX","message":"Enum value should be prefixed"}"#;

        let result = filter_buf_lint_json(input);
        assert!(
            result.starts_with("buf lint: 5 issues across 3 rules"),
            "got: {}",
            result
        );
        // Highest-occurrence rule appears first.
        let snake_pos = result.find("FIELD_LOWER_SNAKE_CASE").expect("rule present");
        let pkg_pos = result.find("PACKAGE_DEFINED").expect("rule present");
        let enum_pos = result.find("ENUM_VALUE_PREFIX").expect("rule present");
        assert!(snake_pos < pkg_pos && snake_pos < enum_pos);
        assert!(result.contains("FIELD_LOWER_SNAKE_CASE (2)"));
        assert!(result.contains("PACKAGE_DEFINED (2)"));
        assert!(result.contains("ENUM_VALUE_PREFIX (1)"));
        assert!(result.contains("foo.proto:12:3"));
        assert!(result.contains("bar.proto:5:1"));
    }

    #[test]
    fn test_filter_lint_top5_and_more() {
        // 7 distinct rules, 1 issue each → expect top 5 + "+2 more rules"
        let lines: Vec<String> = (0..7)
            .map(|i| {
                format!(
                    r#"{{"path":"f{0}.proto","start_line":1,"start_column":1,"end_line":1,"end_column":2,"type":"RULE_{0}","message":"msg {0}"}}"#,
                    i
                )
            })
            .collect();
        let input = lines.join("\n");
        let result = filter_buf_lint_json(&input);
        assert!(result.contains("7 issues across 7 rules"));
        assert!(result.contains("+2 more rules"));
    }

    #[test]
    fn test_filter_lint_corrupted_skipped() {
        let input = r#"{"path":"foo.proto","start_line":12,"start_column":3,"type":"RULE_A","message":"ok"}
{"path":"bar.proto","start_line":5,"start_column":
not-json-line
{"path":"baz.proto","start_line":1,"start_column":1,"type":"RULE_A","message":"ok"}"#;

        let result = filter_buf_lint_json(input);
        // Only 2 lines parse successfully.
        assert!(
            result.starts_with("buf lint: 2 issues across 1 rules"),
            "got: {}",
            result
        );
    }

    #[test]
    fn test_filter_lint_more_samples_per_rule() {
        // Single rule with 5 samples → top 3 + "+2 more"
        let lines: Vec<String> = (0..5)
            .map(|i| {
                format!(
                    r#"{{"path":"f{0}.proto","start_line":{0},"start_column":1,"end_line":{0},"end_column":2,"type":"FIELD_LOWER_SNAKE_CASE","message":"Field name should be lower_snake_case"}}"#,
                    i + 1
                )
            })
            .collect();
        let input = lines.join("\n");
        let result = filter_buf_lint_json(&input);
        assert!(result.contains("FIELD_LOWER_SNAKE_CASE (5)"));
        assert!(result.contains("+2 more"));
    }

    #[test]
    fn test_filter_build_ok() {
        assert_eq!(filter_buf_build(""), "buf build: ok");
        assert_eq!(filter_buf_build("\n  \n"), "buf build: ok");
    }

    #[test]
    fn test_filter_build_errors() {
        let input = r#"foo.proto:12:3: syntax error: unexpected token "}"
bar.proto:5:1: package "foo" already defined"#;
        let result = filter_buf_build(input);
        assert!(result.starts_with("buf build: 2 errors"), "got: {}", result);
        assert!(result.contains("foo.proto:12:3"));
        assert!(result.contains("bar.proto:5:1"));
    }

    #[test]
    fn test_filter_generate_all_success() {
        // Typical buf generate stdout on success.
        let input = "Generating go\nGenerating go-grpc\nGenerating connect-go\n";
        let result = filter_buf_generate(input);
        assert_eq!(result, "buf generate: ok (3 plugins)");
    }

    #[test]
    fn test_filter_generate_silent_success() {
        // Empty stdout also counts as ok.
        assert_eq!(filter_buf_generate(""), "buf generate: ok");
    }

    #[test]
    fn test_filter_generate_with_failure() {
        let input = r#"Generating go
Generating go-grpc
✗ go-grpc: protoc-gen-go-grpc plugin returned exit status 1
error: failed to invoke plugin"#;
        let result = filter_buf_generate(input);
        assert!(
            result.starts_with("buf generate: ") && result.contains("failed"),
            "got: {}",
            result
        );
        assert!(result.contains("✗ go-grpc"));
        assert!(result.contains("exit status 1") || result.contains("failed to invoke plugin"));
    }

    #[test]
    fn test_inject_lint_format_when_missing() {
        let args = vec!["./...".to_string()];
        let out = inject_lint_format(&args);
        assert_eq!(out[0], "--error-format=json");
        assert_eq!(out[1], "./...");
    }

    #[test]
    fn test_inject_lint_format_idempotent_eq() {
        let args = vec!["--error-format=text".to_string(), "./...".to_string()];
        let out = inject_lint_format(&args);
        assert_eq!(out, args);
    }

    #[test]
    fn test_inject_lint_format_idempotent_separated() {
        let args = vec![
            "--error-format".to_string(),
            "json".to_string(),
            "./...".to_string(),
        ];
        let out = inject_lint_format(&args);
        assert_eq!(out, args);
    }
}
