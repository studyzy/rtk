//! Filters gotestsum output using `--jsonfile` interception.
//!
//! Strategy: inject `--jsonfile=<tmpfile>` so gotestsum writes NDJSON test
//! events to a temp file while preserving its own pretty-printed stdout.
//! After execution, read the JSON file and produce a compressed summary via
//! `go_cmd::filter_go_test_json`.
//!
//! Special cases:
//! - User already passes `--jsonfile`: read their file instead of injecting.
//! - `--watch` mode: skip filtering entirely (passthrough) because the tmpfile
//!   accumulates across reruns with no clean boundary.

use anyhow::{Context, Result};
use std::fs;

use crate::cmds::go::go_cmd;
use crate::core::stream::{self, FilterMode, StdinMode};
use crate::core::tracking;
use crate::core::utils::resolved_command;

/// Detect if the user already specified `--jsonfile` in their args.
/// Returns the path if found (supports both `--jsonfile=path` and `--jsonfile path`).
fn extract_jsonfile_path(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(path) = arg.strip_prefix("--jsonfile=") {
            return Some(path.to_string());
        }
        if arg == "--jsonfile" {
            return iter.next().map(|s| s.to_string());
        }
    }
    None
}

/// Detect if `--watch` or `-w` is present (gotestsum watch mode).
fn has_watch_flag(args: &[String]) -> bool {
    args.iter().any(|a| a == "--watch" || a == "-w")
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    // Watch mode: passthrough (tmpfile accumulates indefinitely)
    if has_watch_flag(args) {
        if verbose > 0 {
            eprintln!("gotestsum --watch: passthrough mode (no filtering)");
        }
        let os_args: Vec<std::ffi::OsString> = args.iter().map(std::ffi::OsString::from).collect();
        return crate::core::runner::run_passthrough("gotestsum", &os_args, verbose);
    }

    let timer = tracking::TimedExecution::start();

    // Check if user already specified --jsonfile
    let user_jsonfile = extract_jsonfile_path(args);

    // Build command
    let mut cmd = resolved_command("gotestsum");

    // Inject --jsonfile=<tmpfile> if user didn't provide one
    let tmpfile = if user_jsonfile.is_none() {
        let tf = tempfile::NamedTempFile::new().context("Failed to create temp file for gotestsum JSON output")?;
        cmd.arg(format!("--jsonfile={}", tf.path().display()));
        Some(tf)
    } else {
        None
    };

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: gotestsum {}", args.join(" "));
    }

    // Execute with passthrough (gotestsum prints its own pretty output to terminal)
    let result = stream::run_streaming(&mut cmd, StdinMode::Inherit, FilterMode::Passthrough)
        .context("Failed to run gotestsum")?;

    let exit_code = result.exit_code;

    // Determine JSON file path
    let json_path = if let Some(ref path) = user_jsonfile {
        Some(path.clone())
    } else {
        tmpfile.as_ref().map(|f| f.path().display().to_string())
    };

    // Read JSON and filter
    let filtered = json_path
        .as_deref()
        .and_then(|path| fs::read_to_string(path).ok())
        .filter(|content| !content.trim().is_empty())
        .map(|content| go_cmd::filter_go_test_json(&content));

    match filtered {
        Some(summary) => {
            // Print filtered summary after gotestsum's own output
            println!("\n{}", summary);

            // Track with savings: raw is JSON content, output is filtered summary
            let raw_json = json_path
                .as_deref()
                .and_then(|p| fs::read_to_string(p).ok())
                .unwrap_or_default();
            timer.track(
                &format!("gotestsum {}", args.join(" ")),
                &format!("rtk gotestsum {}", args.join(" ")),
                &raw_json,
                &summary,
            );
        }
        None => {
            // No JSON available — track as passthrough
            timer.track_passthrough(
                &format!("gotestsum {}", args.join(" ")),
                &format!("rtk gotestsum {}", args.join(" ")),
            );
        }
    }

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_jsonfile_path_equals_form() {
        let args = vec!["--jsonfile=/tmp/out.json".to_string(), "--".to_string()];
        assert_eq!(
            extract_jsonfile_path(&args),
            Some("/tmp/out.json".to_string())
        );
    }

    #[test]
    fn test_extract_jsonfile_path_space_form() {
        let args = vec![
            "--jsonfile".to_string(),
            "/tmp/out.json".to_string(),
            "--".to_string(),
        ];
        assert_eq!(
            extract_jsonfile_path(&args),
            Some("/tmp/out.json".to_string())
        );
    }

    #[test]
    fn test_extract_jsonfile_path_absent() {
        let args = vec!["--".to_string(), "-v".to_string(), "./...".to_string()];
        assert_eq!(extract_jsonfile_path(&args), None);
    }

    #[test]
    fn test_extract_jsonfile_path_empty() {
        let args: Vec<String> = Vec::new();
        assert_eq!(extract_jsonfile_path(&args), None);
    }

    #[test]
    fn test_extract_jsonfile_path_dangling_flag() {
        // --jsonfile at end with no value
        let args = vec!["--".to_string(), "--jsonfile".to_string()];
        assert_eq!(extract_jsonfile_path(&args), None);
    }

    #[test]
    fn test_has_watch_flag_long() {
        let args = vec!["--watch".to_string(), "--".to_string()];
        assert!(has_watch_flag(&args));
    }

    #[test]
    fn test_has_watch_flag_short() {
        let args = vec!["-w".to_string(), "--".to_string()];
        assert!(has_watch_flag(&args));
    }

    #[test]
    fn test_has_watch_flag_absent() {
        let args = vec!["--".to_string(), "-v".to_string()];
        assert!(!has_watch_flag(&args));
    }

    #[test]
    fn test_has_watch_flag_empty() {
        let args: Vec<String> = Vec::new();
        assert!(!has_watch_flag(&args));
    }
}
