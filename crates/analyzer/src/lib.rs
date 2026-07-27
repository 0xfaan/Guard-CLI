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
/// `excludes` are glob patterns (e.g. `vendor/**`, `**/generated/*.rs`) matched against each
/// file's path relative to `root`; matching files are skipped entirely.
///
/// `includes` are glob patterns; when non-empty only files matching at least one pattern are
/// scanned. When `includes` is empty all `.rs` files (minus excludes and generated-file
/// headers) are scanned.
pub fn scan_directory(
    root: &Path,
    excludes: &[String],
    includes: &[String],
) -> Result<(Vec<Finding>, usize, usize), ScanError> {
    let root = root.canonicalize()?;

    // Single-file fast path: skip the directory walk entirely.
    if root.is_file() {
        let content = std::fs::read_to_string(&root)?;
        let syn_file = syn::parse_file(&content).map_err(|error| ScanError::Parse {
            path: root.clone(),
            message: error.to_string(),
        })?;
        let file_label = root.file_name().unwrap_or_default().to_string_lossy().to_string();
        let checks = default_checks();
        let mut findings: Vec<Finding> = checks
            .iter()
            .flat_map(|check| {
                let mut hits = check.run(&syn_file, &content);
                for f in &mut hits {
                    f.file_path.clone_from(&file_label);
                }
                hits
            })
            .collect();
        findings.sort_by(|a, b| a.line.cmp(&b.line));
        return Ok((findings, 1, 0));
    }

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
    let include_patterns: Vec<glob::Pattern> = includes
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();

    let entries: Vec<PathBuf> = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            if !entry.file_type().is_file() {
                return false;
            }
            let path = entry.path();
            if path
                .components()
                .any(|c| matches!(c.as_os_str().to_str(), Some("target" | ".git")))
            {
                return false;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                return false;
            }
            let label = path.strip_prefix(&root).unwrap_or(path);
            if exclude_patterns
    let checks = default_checks();

    let filtered: Vec<PathBuf> = paths
        .iter()
        .filter(|path| {
            let label = path.strip_prefix(&root).unwrap_or(path);
            !exclude_patterns
                .iter()
                .any(|p| p.matches_path(label) || p.matches_path(path))
            {
                return false;
            }
            if !include_patterns.is_empty()
                && !include_patterns
                    .iter()
                    .any(|p| p.matches_path(label) || p.matches_path(path))
            {
                return false;
            }
            true
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    let mut files_skipped = 0;
    let mut scan_paths = Vec::new();
    for path in entries {
        if has_generated_file_header(&path)? {
            files_skipped += 1;
            continue;
        }
        scan_paths.push(path);
    }

    // Excludes already applied above; pass empty slice to avoid double-filtering.
    let (findings, files_scanned) = scan_files(&scan_paths, &root, &[])?;

    Ok((findings, files_scanned, files_skipped))
}

/// Scan an explicit list of `.rs` file paths and aggregate findings from every check.
///
/// `root` is used only to compute relative file labels in findings (same convention as
/// [`scan_directory`]). `excludes` are glob patterns matched against each file's path
/// relative to `root`; matching files are skipped.
pub fn scan_files(
    paths: &[PathBuf],
    root: &Path,
    excludes: &[String],
) -> Result<(Vec<Finding>, usize), ScanError> {
    let exclude_patterns: Vec<glob::Pattern> = excludes
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();

    let filtered: Vec<&PathBuf> = paths
        .iter()
        .filter(|path| {
            let label = path.strip_prefix(root).unwrap_or(path.as_path());
            !exclude_patterns
                .iter()
                .any(|pat| pat.matches_path(label) || pat.matches_path(path))
        })
        .collect();

        .cloned()
        .collect();

    let files_scanned = filtered.len();

    let mut findings: Vec<Finding> = filtered
        .par_iter()
        .map(|path| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    eprintln!("warning: skipping {} — permission denied", path.display());
                    return Ok(Vec::new());
                }
                Err(e) => return Err(ScanError::Io(e)),
            };
            let syn_file = syn::parse_file(&content).map_err(|e| ScanError::Parse {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;

            let file_label = path
                .strip_prefix(root)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .to_string();

            let file_findings: Vec<Finding> = checks
                .iter()
                .flat_map(|check| {
                    let check_name = check.name().to_string();
                    match catch_unwind(AssertUnwindSafe(|| check.run(&syn_file, &content))) {
                        Ok(mut hits) => {
                            for f in &mut hits {
                                f.file_path.clone_from(&file_label);
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
                            eprintln!("warning: {}", ScanError::CheckPanic {
                                check: check_name,
                                path: path.to_path_buf(),
                                message,
                            });
                            Vec::new()
                        }
                    }
                })
                .collect();

            Ok(file_findings)
        })
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

    Ok((findings, files_scanned))
}

/// Findings for a single source file.
#[derive(Debug)]
pub struct FileScanResult {
    pub file_path: String,
    pub findings: Vec<Finding>,
}

/// Remove duplicate findings with the same `(file_path, line, check_name)`, keeping the first.
fn dedup_findings(findings: &mut Vec<Finding>) {
    let mut seen = std::collections::HashSet::new();
    findings.retain(|f| seen.insert((f.file_path.clone(), f.line, f.check_name.clone())));
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
///
/// Returns `(results, files_scanned, files_skipped)`, where `files_skipped` counts files
/// skipped because they carry a generated-file header (see [`scan_directory`]).
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

    let mut files_skipped = 0;
    let mut scan_paths = Vec::new();
    for path in paths {
        if has_generated_file_header(&path)? {
            files_skipped += 1;
            continue;
        }
        scan_paths.push(path);
    }

    let files_scanned = scan_paths.len();
    let results: Vec<FileScanResult> = scan_paths
        .par_iter()
        .map(|path| {
            let content = std::fs::read_to_string(path)?;
            let syn_file = syn::parse_file(&content).map_err(|e| ScanError::Parse {
                path: path.clone(),
                message: e.to_string(),
            })?;
            let file_label = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            let mut findings: Vec<Finding> = checks
                .iter()
                .flat_map(|check| {
                    let check_name = check.name().to_string();
                    match catch_unwind(AssertUnwindSafe(|| check.run(&syn_file, &content))) {
                        Ok(mut hits) => {
                            for f in &mut hits {
                                f.file_path.clone_from(&file_label);
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
                                    path: path.clone(),
                                    message,
                                }
                            );
                            Vec::new()
                        }
                    }
                })
                .collect();
            findings.sort_by_key(|f| f.line);
            dedup_findings(&mut findings);
            Ok(FileScanResult { file_path: file_label, findings })
        })
        .collect::<Result<Vec<_>, ScanError>>()?;

    results.sort_by(|a, b| a.file_path.cmp(&b.file_path));
    Ok((results, files_scanned, files_skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn scan_single_rs_file_directly() {
        let dir = std::env::temp_dir().join(format!(
            "soroban-guard-singlefile-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("lib.rs");
        fs::write(&file_path, "pub fn f() {}").unwrap();

        let (_, files_scanned, files_skipped) = scan_directory(&file_path, &[], &[]).unwrap();
        assert_eq!(files_scanned, 1);
        assert_eq!(files_skipped, 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scan_error_check_panic_format() {
        let err = ScanError::CheckPanic {
            check: "example-check".to_string(),
            path: PathBuf::from("src/lib.rs"),
            message: "unexpected AST shape".to_string(),
        };

        assert_eq!(
            err.to_string(),
            "Check `example-check` panicked on src/lib.rs: unexpected AST shape"
        );
    }

    #[test]
    fn reports_scanned_rust_file_count_after_filters() {
        let root = std::env::temp_dir().join(format!(
            "soroban-guard-analyzer-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn included() {}").unwrap();
        fs::write(root.join("src/excluded.rs"), "pub fn excluded() {}").unwrap();
        fs::write(root.join("target/generated.rs"), "pub fn generated() {}").unwrap();
        fs::write(root.join("README.md"), "not Rust").unwrap();

        let (_, files_scanned, files_skipped) =
            scan_directory(&root, &["src/excluded.rs".to_string()], &[]).unwrap();

        assert_eq!(files_scanned, 1);
        assert_eq!(files_skipped, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn include_filter_limits_scanned_files() {
        let root = std::env::temp_dir().join(format!(
            "soroban-guard-include-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn a() {}").unwrap();
        fs::write(root.join("src/other.rs"), "pub fn b() {}").unwrap();

        let (_, files_scanned, files_skipped) =
            scan_directory(&root, &[], &["src/lib.rs".to_string()]).unwrap();

        assert_eq!(files_scanned, 1);
        assert_eq!(files_skipped, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_generated_files_with_header() {
        let root = std::env::temp_dir().join(format!(
            "soroban-guard-generated-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "// @generated\npub fn generated() {}\n",
        )
        .unwrap();

        let (_, files_scanned, files_skipped) = scan_directory(&root, &[], &[]).unwrap();

        assert_eq!(files_scanned, 0);
        assert_eq!(files_skipped, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_files_returns_findings_for_explicit_paths() {
        let root = std::env::temp_dir().join(format!(
            "soroban-guard-scan-files-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        let included = root.join("src/lib.rs");
        let excluded = root.join("src/other.rs");
        fs::write(&included, "pub fn a() {}").unwrap();
        fs::write(&excluded, "pub fn b() {}").unwrap();

        let (_, files_scanned) = scan_files(&[included, excluded.clone()], &root, &[]).unwrap();
        assert_eq!(files_scanned, 2);

        // Exclude one file via glob
        let (_, files_scanned) =
            scan_files(&[excluded], &root, &["src/other.rs".to_string()]).unwrap();
        assert_eq!(files_scanned, 0);

        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod dedup_tests {
    use super::*;
    use soroban_guard_checks::{Finding, Severity};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A check that always returns two identical findings for every file it sees.
    struct DuplicatingCheck;
    impl soroban_guard_checks::Check for DuplicatingCheck {
        fn name(&self) -> &str { "dup-check" }
        fn run(&self, _file: &syn::File, _src: &str) -> Vec<Finding> {
            let f = Finding {
                check_name: "dup-check".into(),
                severity: Severity::Low,
                file_path: String::new(),
                line: 1,
                function_name: "f".into(),
                description: "duplicate".into(),
                rule_url: None,
                suggestion: None,
            };
            vec![f.clone(), f]
        }
    }

    #[test]
    fn deduplicates_findings_with_same_file_line_check() {
        let root = std::env::temp_dir().join(format!(
            "soroban-guard-dedup-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn f() {}").unwrap();

        let checks: Vec<Box<dyn soroban_guard_checks::Check + Send + Sync>> =
            vec![Box::new(DuplicatingCheck)];
        let (results, _, _) = scan_directory_with_checks(&root, &[], &[], &checks).unwrap();

        let total: usize = results.iter().map(|r| r.findings.len()).sum();
        assert_eq!(total, 1, "expected 1 finding after dedup, got {}", total);

        fs::remove_dir_all(root).unwrap();
    }

    /// A check that returns two findings at different lines, intentionally reversed.
    struct ReversedCheck;
    impl soroban_guard_checks::Check for ReversedCheck {
        fn name(&self) -> &str { "reversed-check" }
        fn run(&self, _file: &syn::File, _src: &str) -> Vec<Finding> {
            vec![
                Finding {
                    check_name: "reversed-check".into(),
                    severity: Severity::Low,
                    file_path: String::new(),
                    line: 20,
                    function_name: "b".into(),
                    description: "second".into(),
                    rule_url: None,
                    suggestion: None,
                },
                Finding {
                    check_name: "reversed-check".into(),
                    severity: Severity::Low,
                    file_path: String::new(),
                    line: 5,
                    function_name: "a".into(),
                    description: "first".into(),
                    rule_url: None,
                    suggestion: None,
                },
            ]
        }
    }

    #[test]
    fn findings_sorted_by_file_path_then_line() {
        let root = std::env::temp_dir().join(format!(
            "soroban-guard-sort-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        // Two files — rayon may process them in any order.
        fs::write(root.join("src/b_module.rs"), "pub fn b() {}").unwrap();
        fs::write(root.join("src/a_module.rs"), "pub fn a() {}").unwrap();

        let checks: Vec<Box<dyn soroban_guard_checks::Check + Send + Sync>> =
            vec![Box::new(ReversedCheck)];
        let (results, _, _) = scan_directory_with_checks(&root, &[], &[], &checks).unwrap();

        // Files must be in lexicographic order.
        let file_paths: Vec<&str> = results.iter().map(|r| r.file_path.as_str()).collect();
        assert!(
            file_paths.windows(2).all(|w| w[0] <= w[1]),
            "files not in sorted order: {:?}",
            file_paths
        );

        // Within each file, findings must be sorted by line.
        for r in &results {
            let lines: Vec<usize> = r.findings.iter().map(|f| f.line).collect();
            assert!(
                lines.windows(2).all(|w| w[0] <= w[1]),
                "findings in {} not sorted by line: {:?}",
                r.file_path,
                lines
            );
        }

        fs::remove_dir_all(root).unwrap();
    }
    findings.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then_with(|| a.line.cmp(&b.line))
    });
    dedup_findings(&mut findings);
    Ok((findings, files_scanned, files_skipped))
}
