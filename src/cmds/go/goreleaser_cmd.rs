//! Filters goreleaser release/build/check output — strips progress noise,
//! keeps section headers, failure markers, and success summaries.

use crate::core::runner;
use crate::core::utils::{resolved_command, strip_ansi};
use anyhow::Result;

/// Run goreleaser with output filtering.
///
/// Detects the subcommand (release / build / check / etc.) and applies a
/// shared filter that strips progress noise while preserving section headers,
/// failures, and success summaries.
pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let subcommand = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| "release".to_string());

    let mut cmd = resolved_command("goreleaser");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: goreleaser {}", args.join(" "));
    }

    let sub_for_closure = subcommand.clone();
    runner::run_filtered(
        cmd,
        "goreleaser",
        &args.join(" "),
        move |s: &str| filter_goreleaser(s, &sub_for_closure),
        runner::RunOptions::with_tee("goreleaser"),
    )
}

/// Filter goreleaser output (stdout+stderr combined). Returns a compact
/// summary preserving section headers, failures, and success summaries.
pub(crate) fn filter_goreleaser(input: &str, subcommand: &str) -> String {
    let stripped = strip_ansi(input);

    let mut sections: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut summary: Option<String> = None;
    let mut had_failure = false;

    for line in stripped.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            continue;
        }

        if is_failure_line(trimmed) {
            had_failure = true;
            failures.push(trimmed.trim_start().to_string());
            continue;
        }

        if is_success_summary(trimmed) {
            // Use the most informative summary (last one wins — usually the
            // final "release succeeded after Xs").
            summary = Some(trimmed.trim_start().to_string());
            continue;
        }

        if is_section_header(trimmed) {
            sections.push(trimmed.trim_start().to_string());
            continue;
        }

        if is_progress_line(trimmed) {
            continue;
        }
        // Anything else is dropped as noise (build artifact lines, info
        // messages, etc.). Failures and section headers are the load-bearing
        // signals here.
    }

    let title = if had_failure {
        format!("goreleaser: {} FAILED", subcommand)
    } else {
        format!("goreleaser: {} succeeded", subcommand)
    };

    let mut out = String::new();
    out.push_str(&title);
    out.push('\n');

    for section in &sections {
        out.push_str("  ");
        out.push_str(section);
        out.push('\n');
    }

    for failure in &failures {
        out.push_str("  ");
        out.push_str(failure);
        out.push('\n');
    }

    if let Some(s) = &summary {
        out.push_str("  ");
        out.push_str(s);
        out.push('\n');
    }

    out.trim_end().to_string()
}

/// Progress noise lines — bullet `•` followed by a known verb.
pub(crate) fn is_progress_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('•') {
        return false;
    }
    // Strip the bullet and look at the remaining text (lowercased).
    let rest = trimmed.trim_start_matches('•').trim().to_lowercase();
    const PROGRESS_KEYWORDS: &[&str] = &[
        "building",
        "packaging",
        "signing",
        "publishing",
        "loading",
        "cleaning",
        "running before",
        "snapshotting",
        "caching",
    ];
    PROGRESS_KEYWORDS.iter().any(|kw| rest.starts_with(kw))
}

/// Failure markers — `⨯`, `error:` (case-insensitive), or `FAILED`.
pub(crate) fn is_failure_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('⨯') {
        return true;
    }
    let lower = trimmed.to_lowercase();
    lower.starts_with("error:") || trimmed.contains("FAILED")
}

/// Section header — line begins with `▶`.
pub(crate) fn is_section_header(line: &str) -> bool {
    line.trim_start().starts_with('▶')
}

/// Success summary line — bullet `•` followed by a completion phrase.
pub(crate) fn is_success_summary(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('•') {
        return false;
    }
    let rest = trimmed.trim_start_matches('•').trim().to_lowercase();
    if rest.starts_with("release succeeded")
        || rest.starts_with("ran successfully")
        || rest.contains("succeeded after")
    {
        return true;
    }
    // "built N artifacts" / "built 5 artifact(s)"
    if let Some(after_built) = rest.strip_prefix("built ") {
        if after_built
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
            && after_built.contains("artifact")
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_release_success() {
        let input = "\
  • loading                                       
▶ loading
  • building binaries                             
▶ building binaries (5 platforms)
  • packaging
▶ packaging
  • publishing
▶ publishing
  • release succeeded after 45s
";
        let result = filter_goreleaser(input, "release");
        assert!(
            result.starts_with("goreleaser: release succeeded"),
            "got: {}",
            result
        );
        assert!(result.contains("▶ loading"), "got: {}", result);
        assert!(result.contains("▶ packaging"), "got: {}", result);
        assert!(result.contains("▶ publishing"), "got: {}", result);
        assert!(result.contains("release succeeded after 45s"));
        // Progress lines must be stripped.
        assert!(!result.contains("• loading"));
        assert!(!result.contains("• building binaries"));
        assert!(!result.contains("• packaging\n"));
    }

    #[test]
    fn test_filter_release_failure() {
        let input = "\
▶ building
  • building
⨯ failed to sign artifact: gpg: no secret key
";
        let result = filter_goreleaser(input, "release");
        assert!(
            result.starts_with("goreleaser: release FAILED"),
            "got: {}",
            result
        );
        assert!(result.contains("⨯ failed to sign artifact"));
        assert!(result.contains("▶ building"));
        // Progress noise stripped.
        assert!(!result.contains("• building\n"));
    }

    #[test]
    fn test_filter_build_subcommand() {
        let input = "\
▶ building binaries
  • building
  • built 3 artifacts
";
        let result = filter_goreleaser(input, "build");
        assert!(
            result.starts_with("goreleaser: build succeeded"),
            "got: {}",
            result
        );
        assert!(result.contains("▶ building binaries"));
        assert!(result.contains("built 3 artifacts"));
    }

    #[test]
    fn test_filter_check_subcommand() {
        // `check` has very short output — usually just one success line.
        let input = "  • config is valid\n  • ran successfully\n";
        let result = filter_goreleaser(input, "check");
        assert!(
            result.starts_with("goreleaser: check succeeded"),
            "got: {}",
            result
        );
        assert!(result.contains("ran successfully"));
    }

    #[test]
    fn test_is_progress_line() {
        assert!(is_progress_line("  • building"));
        assert!(is_progress_line("  • packaging"));
        assert!(is_progress_line("  • signing"));
        assert!(is_progress_line("  • publishing"));
        assert!(is_progress_line("  • loading"));
        assert!(is_progress_line("  • cleaning"));
        assert!(is_progress_line("  • running before hooks"));
        assert!(is_progress_line("  • snapshotting"));
        assert!(is_progress_line("  • caching"));
        assert!(!is_progress_line("▶ release"));
        assert!(!is_progress_line("⨯ error"));
        assert!(!is_progress_line("  • release succeeded after 1s"));
    }

    #[test]
    fn test_is_failure_line() {
        assert!(is_failure_line("⨯ failed to sign"));
        assert!(is_failure_line("error: foo"));
        assert!(is_failure_line("Error: bar"));
        assert!(is_failure_line("something FAILED here"));
        assert!(!is_failure_line("▶ release"));
        assert!(!is_failure_line("  • building"));
    }

    #[test]
    fn test_is_section_header() {
        assert!(is_section_header("▶ release"));
        assert!(is_section_header("  ▶ building"));
        assert!(!is_section_header("• building"));
        assert!(!is_section_header("⨯ failed"));
    }

    #[test]
    fn test_filter_strip_ansi() {
        // Input with raw ANSI color escape codes.
        let input = "\x1b[32m▶ loading\x1b[0m\n\x1b[31m⨯ failed: gpg error\x1b[0m\n";
        let result = filter_goreleaser(input, "release");
        // No ANSI codes in output.
        assert!(
            !result.contains('\x1b'),
            "filtered output should not contain ANSI codes, got: {:?}",
            result
        );
        assert!(result.contains("▶ loading"));
        assert!(result.contains("⨯ failed: gpg error"));
        assert!(result.starts_with("goreleaser: release FAILED"));
    }

    #[test]
    fn test_is_success_summary() {
        assert!(is_success_summary("  • release succeeded after 45s"));
        assert!(is_success_summary("  • ran successfully"));
        assert!(is_success_summary("  • built 5 artifacts"));
        assert!(!is_success_summary("  • building"));
        assert!(!is_success_summary("▶ release"));
    }
}
