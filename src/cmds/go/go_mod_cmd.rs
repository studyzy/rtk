//! Filters `go mod` output (tidy/download/graph) — strips noisy progress lines
//! while preserving meaningful changes (added/removed modules, errors, warnings).

use crate::core::runner;
use crate::core::tracking;
use crate::core::utils::{exit_code_from_output, resolved_command, truncate};
use anyhow::{Context, Result};

const MAX_GRAPH_EDGES: usize = 50;
const MAX_DOWNLOAD_FAILURES: usize = 20;
const MAX_LIST_ITEMS: usize = 30;

/// Entry point — dispatches `go mod <subcommand>` to specialised filters or
/// transparent passthrough.
pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    if args.is_empty() {
        anyhow::bail!("go mod: no subcommand specified");
    }

    let sub = args[0].as_str();
    let rest = &args[1..];

    match sub {
        "tidy" => run_tidy(rest, verbose),
        "download" => run_download(rest, verbose),
        "graph" => run_graph(rest, verbose),
        _ => run_passthrough(args, verbose),
    }
}

fn run_tidy(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("go");
    cmd.arg("mod").arg("tidy");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: go mod tidy {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "go mod tidy",
        &args.join(" "),
        filter_mod_tidy,
        runner::RunOptions::with_tee("go_mod_tidy"),
    )
}

fn run_download(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("go");
    cmd.arg("mod").arg("download");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: go mod download {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "go mod download",
        &args.join(" "),
        filter_mod_download,
        runner::RunOptions::with_tee("go_mod_download"),
    )
}

fn run_graph(args: &[String], verbose: u8) -> Result<i32> {
    // Strip RTK-private --deep flag before forwarding to `go mod graph`.
    let deep = args.iter().any(|a| a == "--deep");
    let forwarded: Vec<&String> = args.iter().filter(|a| a.as_str() != "--deep").collect();

    let mut cmd = resolved_command("go");
    cmd.arg("mod").arg("graph");
    for arg in &forwarded {
        cmd.arg(arg);
    }

    if verbose > 0 {
        let display: Vec<&str> = forwarded.iter().map(|s| s.as_str()).collect();
        eprintln!(
            "Running: go mod graph {} (deep={})",
            display.join(" "),
            deep
        );
    }

    let filter: fn(&str) -> String = if deep {
        |s: &str| s.to_string()
    } else {
        filter_mod_graph
    };

    let display_args = forwarded
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ");

    runner::run_filtered(
        cmd,
        "go mod graph",
        &display_args,
        filter,
        runner::RunOptions::with_tee("go_mod_graph"),
    )
}

/// Transparent passthrough for unsupported `go mod` subcommands
/// (init/edit/why/verify/vendor/...). Mirrors the `go_cmd::run_other` pattern:
/// raw stdout/stderr are emitted untouched, while tracking still records the
/// invocation for analytics.
fn run_passthrough(args: &[String], verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    let sub = args[0].as_str();
    let mut cmd = resolved_command("go");
    cmd.arg("mod").arg(sub);
    for arg in &args[1..] {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: go mod {} ...", sub);
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run go mod {}", sub))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let raw = format!("{}\n{}", stdout, stderr);

    print!("{}", stdout);
    eprint!("{}", stderr);

    timer.track(
        &format!("go mod {}", sub),
        &format!("rtk go mod {}", sub),
        &raw,
        &raw,
    );

    Ok(exit_code_from_output(&output, "go mod"))
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// Filter `go mod tidy` output.
///
/// Drops progress noise (`finding`/`downloading`/`extracting`/`upgraded`),
/// keeps `added`/`removed` lines and any error/warning lines.
pub(crate) fn filter_mod_tidy(output: &str) -> String {
    let mut added: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    let mut upgraded: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();

        // Errors / warnings always preserved (checked first so an error line
        // mentioning "downloading" is not silently dropped).
        if is_mod_error_line(&lower) {
            errors.push(trimmed.to_string());
            continue;
        }
        if is_mod_warning_line(&lower) {
            warnings.push(trimmed.to_string());
            continue;
        }

        // Drop noisy progress lines.
        if lower.starts_with("go: finding ")
            || lower.starts_with("go: downloading ")
            || lower.starts_with("go: extracting ")
        {
            continue;
        }

        if lower.starts_with("go: added ") {
            added.push(strip_prefix(trimmed, "go: added "));
        } else if lower.starts_with("go: removed ") {
            removed.push(strip_prefix(trimmed, "go: removed "));
        } else if lower.starts_with("go: upgraded ") {
            upgraded.push(strip_prefix(trimmed, "go: upgraded "));
        }
        // Anything else is non-essential informational output — drop silently.
    }

    let has_changes =
        !added.is_empty() || !removed.is_empty() || !upgraded.is_empty();
    let has_diagnostics = !errors.is_empty() || !warnings.is_empty();

    if !has_changes && !has_diagnostics {
        return "go mod tidy: no changes".to_string();
    }

    let mut result = String::new();
    if has_changes {
        result.push_str(&format!(
            "go mod tidy: {} added, {} removed, {} upgraded\n",
            added.len(),
            removed.len(),
            upgraded.len()
        ));
        result.push_str("═══════════════════════════════════════\n");
        append_list(&mut result, "added", &added);
        append_list(&mut result, "removed", &removed);
        append_list(&mut result, "upgraded", &upgraded);
    } else {
        result.push_str("go mod tidy: no changes\n");
    }

    if !errors.is_empty() {
        result.push_str(&format!("\nerrors ({}):\n", errors.len()));
        for err in errors.iter().take(MAX_LIST_ITEMS) {
            result.push_str(&format!("  {}\n", truncate(err, 120)));
        }
        if errors.len() > MAX_LIST_ITEMS {
            result.push_str(&format!(
                "  … +{} more errors\n",
                errors.len() - MAX_LIST_ITEMS
            ));
        }
    }

    if !warnings.is_empty() {
        result.push_str(&format!("\nwarnings ({}):\n", warnings.len()));
        for w in warnings.iter().take(MAX_LIST_ITEMS) {
            result.push_str(&format!("  {}\n", truncate(w, 120)));
        }
        if warnings.len() > MAX_LIST_ITEMS {
            result.push_str(&format!(
                "  … +{} more warnings\n",
                warnings.len() - MAX_LIST_ITEMS
            ));
        }
    }

    result.trim_end().to_string()
}

/// Filter `go mod download` output.
///
/// Counts `go: downloading <module> <version>` lines, surfaces failures.
pub(crate) fn filter_mod_download(output: &str) -> String {
    let mut downloaded = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();

        if lower.contains("error:") || lower.contains("failed") {
            failures.push(trimmed.to_string());
            continue;
        }

        if lower.starts_with("go: downloading ") {
            downloaded += 1;
            continue;
        }
        // Other progress lines (finding/extracting) are dropped silently.
    }

    let mut result = format!(
        "go mod download: {} modules downloaded, {} failed",
        downloaded,
        failures.len()
    );

    if !failures.is_empty() {
        result.push('\n');
        result.push_str("═══════════════════════════════════════\n");
        for failure in failures.iter().take(MAX_DOWNLOAD_FAILURES) {
            result.push_str(&format!("  {}\n", truncate(failure, 120)));
        }
        if failures.len() > MAX_DOWNLOAD_FAILURES {
            result.push_str(&format!(
                "  … +{} more failures\n",
                failures.len() - MAX_DOWNLOAD_FAILURES
            ));
        }
        result = result.trim_end().to_string();
    }

    result
}

/// Filter `go mod graph` output.
///
/// Default behaviour: dedupe edges, keep only direct dependencies of the root
/// module, truncate at MAX_GRAPH_EDGES with a tee tail hint.
pub(crate) fn filter_mod_graph(output: &str) -> String {
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut parents = std::collections::HashSet::new();
    let mut children = std::collections::HashSet::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Each line: "parent[@ver] child@ver"
        let mut iter = trimmed.split_whitespace();
        let (Some(parent), Some(child)) = (iter.next(), iter.next()) else {
            continue;
        };
        if iter.next().is_some() {
            // Malformed line with extra tokens — skip to stay conservative.
            continue;
        }

        let key = (parent.to_string(), child.to_string());
        if !seen.insert(key.clone()) {
            continue;
        }
        edges.push(key);
        parents.insert(parent.to_string());
        children.insert(child.to_string());
    }

    // Modules count = unique nodes mentioned anywhere.
    let mut modules: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (p, c) in &edges {
        modules.insert(p.as_str());
        modules.insert(c.as_str());
    }

    // The root module appears as a parent but never as a child.
    let roots: Vec<&str> = parents
        .iter()
        .filter(|p| !children.contains(p.as_str()))
        .map(|s| s.as_str())
        .collect();

    let direct: Vec<&(String, String)> = edges
        .iter()
        .filter(|(p, _)| roots.iter().any(|r| r == p))
        .collect();

    if edges.is_empty() {
        return "go mod graph: 0 modules, 0 edges".to_string();
    }

    let mut result = format!(
        "go mod graph: {} modules, {} edges (showing direct deps)\n",
        modules.len(),
        edges.len()
    );
    result.push_str("═══════════════════════════════════════\n");

    let display: Vec<&(String, String)> = if direct.is_empty() {
        // Fall back to all edges when no clear root could be detected.
        edges.iter().collect()
    } else {
        direct
    };

    for (p, c) in display.iter().take(MAX_GRAPH_EDGES) {
        result.push_str(&format!("  {} → {}\n", p, c));
    }

    if display.len() > MAX_GRAPH_EDGES {
        let remaining = display.len() - MAX_GRAPH_EDGES;
        result.push_str(&format!("\n… +{} more edges\n", remaining));

        // Build a textual representation of all edges for the tee hint so the
        // LLM can dump the full graph from disk if needed.
        let all_edges = edges
            .iter()
            .map(|(p, c)| format!("{} {}", p, c))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(hint) = crate::core::tee::force_tee_tail_hint(
            &all_edges,
            "go-mod-graph",
            MAX_GRAPH_EDGES + 1,
        ) {
            result.push_str(&format!("  {}\n", hint));
        }
    }

    result.trim_end().to_string()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_mod_error_line(lower: &str) -> bool {
    lower.contains("error:") || lower.starts_with("error ") || lower.contains(" error ")
}

fn is_mod_warning_line(lower: &str) -> bool {
    lower.starts_with("warning:")
        || lower.starts_with("warning ")
        || lower.contains(" warning:")
}

fn strip_prefix(line: &str, prefix: &str) -> String {
    let lower = line.to_lowercase();
    if lower.starts_with(prefix) {
        line[prefix.len()..].trim().to_string()
    } else {
        line.to_string()
    }
}

fn append_list(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("{} ({}):\n", label, items.len()));
    for item in items.iter().take(MAX_LIST_ITEMS) {
        out.push_str(&format!("  {}\n", truncate(item, 120)));
    }
    if items.len() > MAX_LIST_ITEMS {
        out.push_str(&format!(
            "  … +{} more {}\n",
            items.len() - MAX_LIST_ITEMS,
            label
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_mod_tidy_clean() {
        let output = r#"go: finding module github.com/foo/bar
go: downloading github.com/foo/bar v1.0.0
go: extracting github.com/foo/bar v1.0.0
go: finding module github.com/baz/qux
go: downloading github.com/baz/qux v0.2.1"#;

        let result = filter_mod_tidy(output);
        assert_eq!(result, "go mod tidy: no changes");
    }

    #[test]
    fn test_filter_mod_tidy_with_changes() {
        let output = r#"go: finding module github.com/foo/bar
go: downloading github.com/foo/bar v1.0.0
go: added github.com/foo/bar v1.0.0
go: removed github.com/old/dep v0.5.0
go: upgraded github.com/baz/qux v0.1.0 => v0.2.1"#;

        let result = filter_mod_tidy(output);
        assert!(
            result.starts_with("go mod tidy: 1 added, 1 removed, 1 upgraded"),
            "Expected counts in header, got: {}",
            result
        );
        assert!(result.contains("github.com/foo/bar"));
        assert!(result.contains("github.com/old/dep"));
        assert!(result.contains("github.com/baz/qux"));
    }

    #[test]
    fn test_filter_mod_tidy_keeps_errors() {
        let output = r#"go: finding module github.com/foo/bar
go: downloading github.com/foo/bar v1.0.0
error: missing go.sum entry for github.com/foo/bar
go: warning: module flagged for deprecation"#;

        let result = filter_mod_tidy(output);
        assert!(
            result.contains("missing go.sum entry"),
            "Errors must be preserved, got: {}",
            result
        );
        assert!(
            result.contains("flagged for deprecation"),
            "Warnings must be preserved, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_mod_download_all_success() {
        let output = r#"go: downloading github.com/a/a v1.0.0
go: downloading github.com/b/b v1.1.0
go: downloading github.com/c/c v1.2.0
go: downloading github.com/d/d v1.3.0
go: downloading github.com/e/e v1.4.0"#;

        let result = filter_mod_download(output);
        assert_eq!(result, "go mod download: 5 modules downloaded, 0 failed");
    }

    #[test]
    fn test_filter_mod_download_with_failures() {
        let output = r#"go: downloading github.com/a/a v1.0.0
go: downloading github.com/b/b v1.1.0
error: failed to download github.com/missing/mod v1.0.0: 404 not found
go: downloading github.com/c/c v1.2.0"#;

        let result = filter_mod_download(output);
        assert!(
            result.starts_with("go mod download: 3 modules downloaded, 1 failed"),
            "Wrong header, got: {}",
            result
        );
        assert!(
            result.contains("github.com/missing/mod"),
            "Failure detail must be preserved, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_mod_graph_dedupe() {
        // Same edge listed twice — dedupe should keep it only once.
        let output = r#"root.example/m a@v1.0.0
root.example/m a@v1.0.0
a@v1.0.0 b@v0.5.0"#;

        let result = filter_mod_graph(output);
        // Only one direct edge from root → a.
        assert_eq!(
            result.matches("root.example/m → a@v1.0.0").count(),
            1,
            "Duplicate edges should be deduped, got: {}",
            result
        );
        // Header should report 2 unique edges (root→a, a→b), 3 modules.
        assert!(
            result.contains("3 modules, 2 edges"),
            "Wrong counts in header, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_mod_graph_root_only_default() {
        let output = r#"root.example/m a@v1.0.0
root.example/m b@v1.0.0
a@v1.0.0 c@v0.1.0
b@v1.0.0 d@v0.1.0"#;

        let result = filter_mod_graph(output);
        // Direct deps of root only.
        assert!(result.contains("root.example/m → a@v1.0.0"));
        assert!(result.contains("root.example/m → b@v1.0.0"));
        // Transitive edges should NOT appear in default view.
        assert!(
            !result.contains("a@v1.0.0 → c@v0.1.0"),
            "Transitive edge should be hidden by default, got: {}",
            result
        );
        assert!(
            !result.contains("b@v1.0.0 → d@v0.1.0"),
            "Transitive edge should be hidden by default, got: {}",
            result
        );
    }

    #[test]
    fn test_filter_mod_graph_truncate() {
        // Build > 50 direct edges from a single root.
        let mut lines = Vec::new();
        for i in 0..MAX_GRAPH_EDGES + 5 {
            lines.push(format!("root.example/m dep{}@v1.0.0", i));
        }
        let output = lines.join("\n");

        let result = filter_mod_graph(&output);
        assert!(
            result.contains(&format!(
                "… +{} more edges",
                (MAX_GRAPH_EDGES + 5) - MAX_GRAPH_EDGES
            )),
            "Truncation marker missing, got: {}",
            result
        );
        // Should NOT include the 51st (zero-indexed dep50) in the visible list.
        // (We can only verify the marker; tee hint is best-effort because it
        // requires writable tee storage.)
        let visible_count = result.matches("root.example/m → dep").count();
        assert_eq!(
            visible_count, MAX_GRAPH_EDGES,
            "Should display exactly {} edges, got {} in: {}",
            MAX_GRAPH_EDGES, visible_count, result
        );
    }

    #[test]
    fn test_filter_mod_download_empty() {
        let result = filter_mod_download("");
        assert_eq!(result, "go mod download: 0 modules downloaded, 0 failed");
    }

    #[test]
    fn test_filter_mod_graph_empty() {
        let result = filter_mod_graph("");
        assert_eq!(result, "go mod graph: 0 modules, 0 edges");
    }
}
