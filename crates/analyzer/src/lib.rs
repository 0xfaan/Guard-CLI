//! Walk Rust sources, parse with `syn`, and run registered checks.
//!
//! Each [`Check`](soroban_guard_checks::Check) runs independently on the same parsed file;
//! findings are concatenated with no shared mutable state between checks.

use rayon::prelude::*;
use soroban_guard_checks::{default_checks, Check, Finding};
use std::collections::HashSet;
use std::io::BufRead;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use thiserror::Error;
use walkdir::WalkDir;

const SUPPRESSION_PREFIX: &str = "// soroban-guard: allow(";

#[derive(Error, Debug)]
pub enum ScanError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Permission denied reading {path}")]
    PermissionDenied { path: PathBuf },
    #[error("Failed to parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("Check `{check}` panicked on {path}: {message}")]
    CheckPanic {
        check: String,
        path: PathBuf,
        message: String,
    },
}

#[derive(Default)]
struct Suppressions {
    line_checks: HashSet<(usize, String)>,
    function_checks: HashSet<(String, String)>,
}

fn has_generated_file_header(path: &Path) -> Result<bool, std::io::Error> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();

    for _ in 0..5 {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("// @generated")
            || trimmed.starts_with("// Code generated")
            || trimmed.starts_with("// DO NOT EDIT")
        {
            return Ok(true);
        }
    }

    Ok(false)
}

fn parse_allow_checks(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix(SUPPRESSION_PREFIX)?;
    let (inside, _) = rest.split_once(')')?;
    let checks: Vec<String> = inside
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    (!checks.is_empty()).then_some(checks)
}

fn function_name_from_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let fn_pos = trimmed.find("fn ")?;
    let after_fn = &trimmed[fn_pos + 3..];
    let name: String = after_fn
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

fn parse_suppressions(source: &str) -> Suppressions {
    let lines: Vec<&str> = source.lines().collect();
    let mut suppressions = Suppressions::default();

    for (idx, line) in lines.iter().enumerate() {
        let Some(checks) = parse_allow_checks(line) else {
            continue;
        };
        let target_idx = idx + 1;
        let Some(target_line) = lines.get(target_idx) else {
            continue;
        };
        if let Some(function_name) = function_name_from_line(target_line) {
            for check in checks {
                suppressions
                    .function_checks
                    .insert((function_name.clone(), check));
            }
        } else {
            let target_line_number = target_idx + 1;
            for check in checks {
                suppressions.line_checks.insert((target_line_number, check));
            }
        }
    }

    suppressions
}

fn is_suppressed(finding: &Finding, suppressions: &Suppressions) -> bool {
    suppressions
        .line_checks
        .contains(&(finding.line, finding.check_name.clone()))
        || suppressions
            .function_checks
            .contains(&(finding.function_name.clone(), finding.check_name.clone()))
}

fn dedup_findings(findings: &mut Vec<Finding>) {
    let mut seen = HashSet::new();
    findings.retain(|f| seen.insert((f.file_path.clone(), f.line, f.check_name.clone())));
}

fn collect_rust_paths(
    root: &Path,
    excludes: &[String],
    includes: &[String],
) -> Result<(Vec<PathBuf>, usize), ScanError> {
    let exclude_patterns: Vec<glob::Pattern> = excludes
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();
    let include_patterns: Vec<glob::Pattern> = includes
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();

    if root.is_file() {
        return Ok((vec![root.to_path_buf()], 0));
    }

    let mut files_skipped = 0;
    let mut paths = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some("target" | ".git")))
        {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let label = path.strip_prefix(root).unwrap_or(path);
        if exclude_patterns
            .iter()
            .any(|p| p.matches_path(label) || p.matches_path(path))
        {
            continue;
        }
        if !include_patterns.is_empty()
            && !include_patterns
                .iter()
                .any(|p| p.matches_path(label) || p.matches_path(path))
        {
            continue;
        }
        if has_generated_file_header(path)? {
            files_skipped += 1;
            continue;
        }
        paths.push(path.to_path_buf());
    }

    Ok((paths, files_skipped))
}

fn run_checks_for_file(
    path: &Path,
    root: &Path,
    checks: &[Box<dyn Check + Send + Sync>],
) -> Result<Vec<Finding>, ScanError> {
    let content = std::fs::read_to_string(path)?;
    let syn_file = syn::parse_file(&content).map_err(|e| ScanError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let file_label = if root.is_file() {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    } else {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    };
    let suppressions = parse_suppressions(&content);

    let mut findings: Vec<Finding> = checks
        .iter()
        .flat_map(|check| {
            let check_name = check.name().to_string();
            match catch_unwind(AssertUnwindSafe(|| check.run(&syn_file, &content))) {
                Ok(mut hits) => {
                    for finding in &mut hits {
                        finding.file_path.clone_from(&file_label);
                    }
                    hits
                }
                Err(payload) => {
                    let message = if let Some(msg) = payload.downcast_ref::<&str>() {
                        msg.to_string()
                    } else if let Some(msg) = payload.downcast_ref::<String>() {
                        msg.clone()
                    } else {
                        "panic payload was not a string".to_string()
                    };
                    eprintln!(
                        "warning: {}",
                        ScanError::CheckPanic {
                            check: check_name,
                            path: path.to_path_buf(),
                            message,
                        }
                    );
                    Vec::new()
                }
            }
        })
        .filter(|finding| !is_suppressed(finding, &suppressions))
        .collect();

    findings.sort_by_key(|f| f.line);
    dedup_findings(&mut findings);
    Ok(findings)
}

/// `root` is used to compute relative file labels in findings. `excludes` are glob patterns
/// matched against each explicit file path relative to `root`.
/// Recursively scan `.rs` files under `root` and aggregate findings from every check.
///
/// `root` may be a directory **or a single `.rs` file**. When a file path is given it is scanned
/// directly without any directory walk.
///
/// `root` is used only to compute relative file labels in findings (same convention as
/// [`scan_directory`]). `excludes` are glob patterns matched against each file's path
/// relative to `root`; matching files are skipped.
pub fn scan_files(
    paths: &[PathBuf],
    root: &Path,
    excludes: &[String],
) -> Result<(Vec<Finding>, usize), ScanError> {
    let root = root.canonicalize()?;
    let exclude_patterns: Vec<glob::Pattern> = excludes
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();
    let checks = default_checks();

    let filtered: Vec<PathBuf> = paths
        .iter()
        .filter(|path| {
            let label = path.strip_prefix(&root).unwrap_or(path);
            !exclude_patterns
                .iter()
                .any(|pat| pat.matches_path(label) || pat.matches_path(path))
        })
        .cloned()
        .collect();

    let files_scanned = filtered.len();

    let mut findings: Vec<Finding> = filtered
        .par_iter()
        .map(|path| run_checks_for_file(path, &root, &checks))
        .collect::<Result<Vec<Vec<Finding>>, ScanError>>()?
        .into_iter()
        .flatten()
        .collect();

    findings.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then_with(|| a.line.cmp(&b.line))
    });
    dedup_findings(&mut findings);
    Ok((findings, files_scanned))
}

/// Recursively scan `.rs` files under `root` and aggregate findings from every default check.
///
/// `root` may be a directory or a single `.rs` file. `excludes` and `includes` are glob patterns
/// matched against paths relative to `root`. Generated Rust files with common generated-file
/// headers are skipped.
pub fn scan_directory(
    root: &Path,
    excludes: &[String],
    includes: &[String],
) -> Result<(Vec<Finding>, usize, usize), ScanError> {
    let root = root.canonicalize()?;
    let checks = default_checks();
    let (paths, files_skipped) = collect_rust_paths(&root, excludes, includes)?;
    let files_scanned = paths.len();

    let mut findings: Vec<Finding> = paths
        .par_iter()
        .map(|path| run_checks_for_file(path, &root, &checks))
        .collect::<Result<Vec<Vec<Finding>>, ScanError>>()?
        .into_iter()
        .flatten()
        .collect();

    findings.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then_with(|| a.line.cmp(&b.line))
    });
    dedup_findings(&mut findings);
    Ok((findings, files_scanned, files_skipped))
}

/// Like [`scan_directory`] but runs `checks` instead of [`default_checks`].
pub fn scan_directory_with_checks(
    root: &Path,
    excludes: &[String],
    includes: &[String],
    checks: &[Box<dyn Check + Send + Sync>],
) -> Result<(Vec<Finding>, usize, usize), ScanError> {
    let root = root.canonicalize()?;
    let (paths, files_skipped) = collect_rust_paths(&root, excludes, includes)?;
    let files_scanned = paths.len();

    let mut findings: Vec<Finding> = paths
        .par_iter()
        .map(|path| run_checks_for_file(path, &root, checks))
        .collect::<Result<Vec<Vec<Finding>>, ScanError>>()?
        .into_iter()
        .flatten()
        .collect();

    findings.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then_with(|| a.line.cmp(&b.line))
    });
    dedup_findings(&mut findings);
    Ok((findings, files_scanned, files_skipped))
}
