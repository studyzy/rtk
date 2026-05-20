//! Filters `go list` output — packages and modules summaries.

use crate::core::runner;
use crate::core::utils::resolved_command;
use anyhow::Result;
use serde_json::Value;

const MAX_GO_LIST_ENTRIES: usize = 10;

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let (stripped, deep) = strip_deep_flag(args);
    let modules_mode = detect_modules_mode(&stripped);

    let user_has_json = stripped.iter().any(|a| a == "-json" || a.starts_with("-json="));

    let mut cmd = resolved_command("go");
    cmd.arg("list");

    if !deep && !user_has_json {
        cmd.arg("-json");
    }

    for arg in &stripped {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!(
            "Running: go list {}{}",
            if !deep && !user_has_json { "-json " } else { "" },
            stripped.join(" ")
        );
    }

    let filter: fn(&str) -> String = if deep {
        // Pass-through: user explicitly opted out of filtering.
        |s: &str| s.to_string()
    } else if modules_mode {
        filter_go_list_modules
    } else {
        filter_go_list_packages
    };

    runner::run_filtered(
        cmd,
        "go list",
        &stripped.join(" "),
        filter,
        runner::RunOptions::stdout_only().tee("go_list"),
    )
}

/// Strip the RTK-private `--deep` flag from args.
/// Returns (cleaned_args, deep_was_set).
fn strip_deep_flag(args: &[String]) -> (Vec<String>, bool) {
    let mut deep = false;
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    for arg in args {
        if arg == "--deep" {
            deep = true;
        } else {
            out.push(arg.clone());
        }
    }
    (out, deep)
}

/// Detect whether the user requested module mode (`-m`).
fn detect_modules_mode(args: &[String]) -> bool {
    args.iter().any(|a| a == "-m")
}

/// Parse `go list -json` package object stream and produce a compact summary.
pub(crate) fn filter_go_list_packages(input: &str) -> String {
    let mut entries: Vec<String> = Vec::new();

    let stream = serde_json::Deserializer::from_str(input).into_iter::<Value>();
    for item in stream {
        let value = match item {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(import_path) = value
            .get("ImportPath")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            entries.push(import_path);
        } else if let Some(name) = value
            .get("Name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            entries.push(name);
        } else if let Some(dir) = value
            .get("Dir")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            entries.push(dir);
        }
    }

    if entries.is_empty() {
        return "go list: no packages".to_string();
    }

    let total = entries.len();
    let mut result = String::new();
    result.push_str(&format!("go list: {} packages\n", total));
    result.push_str("═══════════════════════════════════════\n");

    for (i, entry) in entries.iter().take(MAX_GO_LIST_ENTRIES).enumerate() {
        result.push_str(&format!("{}. {}\n", i + 1, entry));
    }

    if total > MAX_GO_LIST_ENTRIES {
        let remaining = total - MAX_GO_LIST_ENTRIES;
        result.push_str(&format!("\n… +{} more\n", remaining));
        let all = entries.join("\n");
        if let Some(hint) =
            crate::core::tee::force_tee_tail_hint(&all, "go-list", MAX_GO_LIST_ENTRIES + 1)
        {
            result.push_str(&format!("  {}\n", hint));
        }
    }

    result.trim().to_string()
}

/// Parse `go list -m -json` module object stream and produce a compact summary.
pub(crate) fn filter_go_list_modules(input: &str) -> String {
    struct Entry {
        path: String,
        version: Option<String>,
        main: bool,
    }

    let mut entries: Vec<Entry> = Vec::new();

    let stream = serde_json::Deserializer::from_str(input).into_iter::<Value>();
    for item in stream {
        let value = match item {
            Ok(v) => v,
            Err(_) => continue,
        };

        let path = match value.get("Path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => continue,
        };
        let version = value
            .get("Version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let main = value
            .get("Main")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        entries.push(Entry {
            path,
            version,
            main,
        });
    }

    if entries.is_empty() {
        return "go list: no modules".to_string();
    }

    let total = entries.len();
    let mut result = String::new();
    result.push_str(&format!("go list: {} modules\n", total));
    result.push_str("═══════════════════════════════════════\n");

    let format_entry = |e: &Entry| -> String {
        let marker = if e.main { "★ " } else { "" };
        match &e.version {
            Some(v) => format!("{}{}@{}", marker, e.path, v),
            None => format!("{}{}", marker, e.path),
        }
    };

    for (i, entry) in entries.iter().take(MAX_GO_LIST_ENTRIES).enumerate() {
        result.push_str(&format!("{}. {}\n", i + 1, format_entry(entry)));
    }

    if total > MAX_GO_LIST_ENTRIES {
        let remaining = total - MAX_GO_LIST_ENTRIES;
        result.push_str(&format!("\n… +{} more\n", remaining));
        let all: String = entries
            .iter()
            .map(format_entry)
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(hint) =
            crate::core::tee::force_tee_tail_hint(&all, "go-list", MAX_GO_LIST_ENTRIES + 1)
        {
            result.push_str(&format!("  {}\n", hint));
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_packages_simple() {
        // Object stream (no separators between objects)
        let input = r#"{"ImportPath":"example.com/foo","Name":"foo","Dir":"/tmp/foo"}{"ImportPath":"example.com/bar","Name":"bar","Dir":"/tmp/bar"}{"ImportPath":"example.com/baz","Name":"baz","Dir":"/tmp/baz"}"#;

        let result = filter_go_list_packages(input);
        assert!(result.contains("3 packages"), "got: {}", result);
        assert!(result.contains("example.com/foo"));
        assert!(result.contains("example.com/bar"));
        assert!(result.contains("example.com/baz"));
    }

    #[test]
    fn test_filter_modules_simple() {
        let input = r#"{"Path":"example.com/myapp","Main":true,"GoVersion":"1.22"}{"Path":"github.com/pkg/errors","Version":"v0.9.1"}{"Path":"golang.org/x/sync","Version":"v0.5.0"}"#;

        let result = filter_go_list_modules(input);
        assert!(result.contains("3 modules"), "got: {}", result);
        // Main module marked with ★
        assert!(result.contains("★ example.com/myapp"), "got: {}", result);
        assert!(result.contains("github.com/pkg/errors@v0.9.1"));
        assert!(result.contains("golang.org/x/sync@v0.5.0"));
    }

    #[test]
    fn test_filter_packages_truncate() {
        let mut input = String::new();
        for i in 0..15 {
            input.push_str(&format!(
                r#"{{"ImportPath":"example.com/pkg{}","Name":"pkg{}"}}"#,
                i, i
            ));
        }

        let result = filter_go_list_packages(&input);
        assert!(result.contains("15 packages"), "got: {}", result);
        assert!(result.contains("example.com/pkg0"));
        assert!(result.contains("example.com/pkg9"));
        // Should NOT contain entry #11 (index 10) directly listed
        assert!(!result.contains("11. example.com/pkg10"));
        assert!(result.contains("+5 more"), "got: {}", result);
        // tee hint should be present (force_tee_tail_hint formats as "[see remaining: ...")
        assert!(result.contains("[see remaining"), "got: {}", result);
    }

    #[test]
    fn test_filter_packages_empty() {
        let result = filter_go_list_packages("");
        assert_eq!(result, "go list: no packages");
    }

    #[test]
    fn test_filter_packages_corrupted_skipped() {
        // First object is valid, then corrupted JSON, then another valid object.
        // serde_json's streaming Deserializer stops at the first error, so any
        // tail after a corruption is dropped — but the leading valid object is
        // still counted.
        let input = r#"{"ImportPath":"example.com/foo","Name":"foo"}{"ImportPath":"exam"#;

        let result = filter_go_list_packages(input);
        assert!(result.contains("1 packages"), "got: {}", result);
        assert!(result.contains("example.com/foo"));
    }

    #[test]
    fn test_filter_modules_empty() {
        let result = filter_go_list_modules("");
        assert_eq!(result, "go list: no modules");
    }

    #[test]
    fn test_filter_modules_truncate() {
        let mut input = String::new();
        for i in 0..12 {
            input.push_str(&format!(
                r#"{{"Path":"example.com/mod{}","Version":"v0.0.{}"}}"#,
                i, i
            ));
        }

        let result = filter_go_list_modules(&input);
        assert!(result.contains("12 modules"), "got: {}", result);
        assert!(result.contains("example.com/mod0@v0.0.0"));
        assert!(result.contains("example.com/mod9@v0.0.9"));
        assert!(result.contains("+2 more"), "got: {}", result);
        assert!(result.contains("[see remaining"), "got: {}", result);
    }

    #[test]
    fn test_detect_modules_mode() {
        assert!(detect_modules_mode(&[
            "-m".to_string(),
            "all".to_string()
        ]));
        assert!(detect_modules_mode(&["-m".to_string()]));
        assert!(!detect_modules_mode(&["./...".to_string()]));
        assert!(!detect_modules_mode(&[]));
        assert!(!detect_modules_mode(&["-mod=vendor".to_string()]));
    }

    #[test]
    fn test_strip_deep_flag() {
        let (out, deep) = strip_deep_flag(&["--deep".to_string(), "./...".to_string()]);
        assert_eq!(out, vec!["./...".to_string()]);
        assert!(deep);

        let (out, deep) = strip_deep_flag(&["./...".to_string(), "-m".to_string()]);
        assert_eq!(out, vec!["./...".to_string(), "-m".to_string()]);
        assert!(!deep);

        let (out, deep) = strip_deep_flag(&[]);
        assert!(out.is_empty());
        assert!(!deep);

        // --deep can appear in any position
        let (out, deep) = strip_deep_flag(&[
            "-m".to_string(),
            "--deep".to_string(),
            "all".to_string(),
        ]);
        assert_eq!(out, vec!["-m".to_string(), "all".to_string()]);
        assert!(deep);
    }
}
