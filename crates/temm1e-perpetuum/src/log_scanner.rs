//! Log scanner — reads temm1e.log and groups errors by signature.
//!
//! Uses streaming BufReader (O(unique signatures), not O(file size)).
//! Caps at MAX_ERROR_GROUPS to bound memory.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Maximum error groups to track in a single scan.
const MAX_ERROR_GROUPS: usize = 50;

/// A group of identical errors found in the log.
#[derive(Debug, Clone)]
pub struct ErrorGroup {
    /// Error signature: "{location}:{message_prefix}".
    pub signature: String,
    /// Full first occurrence of the error message.
    pub message: String,
    /// Source file:line if available.
    pub location: Option<String>,
    /// Number of occurrences in the scan window.
    pub count: u32,
    /// Timestamps of occurrences (ISO 8601, max 10).
    pub timestamps: Vec<String>,
    /// Raw log lines (max 5, for context).
    pub sample_lines: Vec<String>,
}

/// Scan log file for ERROR/WARN/panic entries within the last `hours`.
///
/// Returns groups with count >= `min_count`, sorted by frequency (most first).
/// Excludes errors from the bug reporter itself (self-referential prevention).
pub fn scan_recent_errors(log_path: &Path, hours: u32, min_count: u32) -> Vec<ErrorGroup> {
    let file = match std::fs::File::open(log_path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
    let mut groups: HashMap<String, ErrorGroup> = HashMap::new();

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };

        // Parse JSON log line
        let parsed: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Check level
        let level = parsed.get("level").and_then(|l| l.as_str()).unwrap_or("");
        if level != "ERROR" && level != "WARN" {
            continue;
        }

        // Exclude bug reporter's own errors (self-referential prevention)
        let target = parsed.get("target").and_then(|t| t.as_str()).unwrap_or("");
        if target.contains("bug_reporter") || target.contains("self_work") {
            continue;
        }

        // Check timestamp within window (include on parse failure — safe default)
        let ts = parsed
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
            if dt < cutoff {
                continue;
            }
        }

        // Extract error info
        let message = parsed
            .get("fields")
            .and_then(|f| f.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();

        let location = parsed
            .get("fields")
            .and_then(|f| f.get("panic.location"))
            .and_then(|l| l.as_str())
            .map(String::from);

        // Build signature
        let loc_str = location.as_deref().unwrap_or("unknown");
        let msg_prefix: String = message.chars().take(60).collect();
        let signature = format!("{}:{}", loc_str, msg_prefix);

        // Group (cap total groups)
        if !groups.contains_key(&signature) && groups.len() >= MAX_ERROR_GROUPS {
            continue; // At capacity — skip new signatures
        }

        let group = groups
            .entry(signature.clone())
            .or_insert_with(|| ErrorGroup {
                signature,
                message: message.clone(),
                location: location.clone(),
                count: 0,
                timestamps: Vec::new(),
                sample_lines: Vec::new(),
            });
        group.count += 1;
        if group.timestamps.len() < 10 {
            group.timestamps.push(ts.to_string());
        }
        if group.sample_lines.len() < 5 {
            group.sample_lines.push(line.clone());
        }
    }

    let mut result: Vec<ErrorGroup> = groups
        .into_values()
        .filter(|g| g.count >= min_count)
        .collect();
    result.sort_by_key(|e| std::cmp::Reverse(e.count));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_error_line(f: &mut std::fs::File, msg: &str, location: Option<&str>) {
        let now = chrono::Utc::now().to_rfc3339();
        let loc_field = location
            .map(|l| format!(r#","panic.location":"{}""#, l))
            .unwrap_or_default();
        writeln!(
            f,
            r#"{{"timestamp":"{}","level":"ERROR","target":"temm1e_agent","fields":{{"message":"{}"{}}}}}"#,
            now, msg, loc_field
        )
        .unwrap();
    }

    #[test]
    fn scan_empty_file() {
        let dir = std::env::temp_dir().join("temm1e_scan_empty");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test.log");
        std::fs::write(&path, "").unwrap();
        let groups = scan_recent_errors(&path, 6, 2);
        assert!(groups.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_groups_by_signature() {
        let dir = std::env::temp_dir().join("temm1e_scan_groups");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test.log");
        let mut f = std::fs::File::create(&path).unwrap();
        for _ in 0..3 {
            write_error_line(&mut f, "test error", Some("src/foo.rs:42:1"));
        }
        let groups = scan_recent_errors(&path, 6, 2);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].count, 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_ignores_old_entries() {
        let dir = std::env::temp_dir().join("temm1e_scan_old");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test.log");
        let old = (chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
        let mut f = std::fs::File::create(&path).unwrap();
        for _ in 0..3 {
            writeln!(
                f,
                r#"{{"timestamp":"{}","level":"ERROR","target":"temm1e_agent","fields":{{"message":"old error"}}}}"#,
                old
            )
            .unwrap();
        }
        let groups = scan_recent_errors(&path, 6, 2);
        assert!(groups.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_filters_below_min_count() {
        let dir = std::env::temp_dir().join("temm1e_scan_min");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test.log");
        let mut f = std::fs::File::create(&path).unwrap();
        write_error_line(&mut f, "single error", None);
        let groups = scan_recent_errors(&path, 6, 2);
        assert!(groups.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_excludes_self_work_target() {
        let dir = std::env::temp_dir().join("temm1e_scan_self");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test.log");
        let now = chrono::Utc::now().to_rfc3339();
        let mut f = std::fs::File::create(&path).unwrap();
        for _ in 0..3 {
            writeln!(
                f,
                r#"{{"timestamp":"{}","level":"ERROR","target":"self_work","fields":{{"message":"reporter error"}}}}"#,
                now
            )
            .unwrap();
        }
        let groups = scan_recent_errors(&path, 6, 2);
        assert!(groups.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_nonexistent_file_returns_empty() {
        let groups = scan_recent_errors(Path::new("/nonexistent/path.log"), 6, 2);
        assert!(groups.is_empty());
    }
}
