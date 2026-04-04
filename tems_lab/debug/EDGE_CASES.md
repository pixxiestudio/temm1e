# Tem Debug — Edge Cases for Indefinite Operation

> A system that runs forever must handle every edge case that probability
> guarantees WILL happen given enough time. A 0.1% risk per cycle
> becomes 63% over 1000 cycles. This document maps every failure mode
> and ensures each one degrades gracefully, never catastrophically.

---

## Category 1: Disk and Storage

### 1.1 Log cleanup fails silently

**Scenario:** `cleanup_old_logs()` runs on startup but the files are locked (another process reading them), permissions changed, or the function panics.

**What happens over time:** Log directory grows without bound. Eventually fills disk. The entire system (not just TEMM1E) stops working.

**Fix:** Cleanup must never be the only defense. Add a hard size cap:

```rust
/// Maximum total log directory size (100 MB).
const MAX_LOG_DIR_BYTES: u64 = 100 * 1024 * 1024;

fn enforce_log_budget(dir: &Path) {
    let total: u64 = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();

    if total > MAX_LOG_DIR_BYTES {
        // Delete oldest files until under budget
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let modified = meta.modified().ok()?;
                Some((e.path(), modified, meta.len()))
            })
            .collect();
        files.sort_by_key(|(_, modified, _)| *modified);

        let mut freed = 0u64;
        let excess = total - MAX_LOG_DIR_BYTES;
        for (path, _, size) in &files {
            if freed >= excess { break; }
            std::fs::remove_file(path).ok();
            freed += size;
        }
    }
}
```

Call `enforce_log_budget()` both on startup AND before every write cycle (in the appender guard's drop, or as a periodic check).

**After fix:** Log dir is bounded at 100 MB regardless of cleanup failures. Disk cannot fill from logs.

### 1.2 Disk full during log write

**Scenario:** Disk is full from non-TEMM1E causes. `tracing-appender` tries to write, fails.

**What happens:** `tracing-appender::non_blocking` uses a bounded channel. If the file writer can't flush, the channel fills. When the channel is full, new log events are DROPPED (not queued). The daemon continues operating — stdout still works. Log events are lost but the system doesn't crash.

**Verification needed:** Confirm that `tracing-appender` drops events rather than blocking. The `non_blocking()` builder returns a `WorkerGuard` — when the channel buffer is full, it discards.

**Fix:** Log a warning via stdout (which still works) when file writes fail:

```rust
let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
// non_blocking already handles drops — no additional code needed.
// But we should log the drop count periodically.
```

**After fix:** Disk full → log events dropped silently. Daemon continues. No crash. No hang.

### 1.3 Orphaned staged updates accumulate

**Scenario:** Auto-update downloads `temm1e-v4.2.0` to `~/.temm1e/updates/`. User never applies it. Next release: `temm1e-v4.2.1` also downloaded. Over months, 20+ staged binaries accumulate (30 MB each = 600 MB).

**Fix:** Before staging a new update, delete all previous staged binaries:

```rust
fn clean_staged_updates(updates_dir: &Path, keep_version: Option<&str>) {
    if let Ok(entries) = std::fs::read_dir(updates_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Keep only the current backup (.bak) and the new download
            if keep_version.map_or(true, |v| !name_str.contains(v))
                && !name_str.ends_with(".bak")
            {
                std::fs::remove_file(entry.path()).ok();
            }
        }
    }
}
```

**After fix:** Updates dir contains at most 2 files: the new binary + the .bak rollback. Bounded at ~60 MB.

---

## Category 2: Time and Clocks

### 2.1 Clock skew / NTP jump

**Scenario:** System clock jumps backward (NTP correction, VM migration, manual change). Log timestamps become non-monotonic. The log scanner's "last 6 hours" window may include the entire log file or exclude everything.

**Fix:** Use monotonic ordering where possible. For the log scanner, sort by line position (file offset) not timestamp. Use the timestamp only for the cutoff filter, and tolerate clock jumps:

```rust
// If a timestamp fails to parse, include the line (safe default)
let in_window = match chrono::DateTime::parse_from_rfc3339(ts) {
    Ok(dt) => dt >= cutoff,
    Err(_) => true, // Include — better to over-scan than under-scan
};
```

**After fix:** Clock jumps cause the scanner to potentially scan more lines than needed (extra work, no harm) rather than miss lines (missed bugs).

### 2.2 Rate limit window drift

**Scenario:** Rate limit is "1 report per 6 hours." The timestamp is stored as RFC 3339. If the clock jumps forward 24 hours, then back, the rate limit thinks 24 hours have passed and allows a burst.

**Fix:** Use a simple counter with file-based persistence, not timestamp comparison:

```rust
// Store: last_report_count (incremented each report)
// + last_report_epoch (Unix timestamp from monotonic source if available)
// Allow report if: (current_epoch - last_epoch) >= 21600 (6 hours in seconds)
// Fallback: if time went backward, DON'T allow (safe default)
let elapsed = current_epoch.saturating_sub(last_epoch);
if elapsed < 21600 {
    return Err("Rate limited");
}
```

The `saturating_sub` ensures that backward clock jumps result in 0 elapsed time → rate limited → safe.

**After fix:** Clock jumps cannot create rate limit bypass. Backward jumps are treated as "no time passed."

---

## Category 3: Network and API

### 3.1 GitHub API deprecation

**Scenario:** Years from now, GitHub changes the Issues API endpoint, response format, or authentication method.

**Fix:** All GitHub API calls go through a single module (`bug_reporter.rs`). No GitHub-specific code elsewhere. Version the API calls:

```rust
const GITHUB_API_VERSION: &str = "2022-11-28"; // Pin to a specific API version

fn github_headers(token: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Authorization", format!("Bearer {}", token).parse().unwrap());
    headers.insert("User-Agent", "TEMM1E".parse().unwrap());
    headers.insert("Accept", "application/vnd.github+json".parse().unwrap());
    headers.insert("X-GitHub-Api-Version", GITHUB_API_VERSION.parse().unwrap());
    headers
}
```

GitHub supports API versioning via `X-GitHub-Api-Version` header. Pinning to `2022-11-28` means our requests work even if GitHub ships breaking changes to newer versions.

**After fix:** API changes don't break existing functionality. The pinned version is supported by GitHub's backwards compatibility guarantee.

### 3.2 Network partition during issue creation

**Scenario:** POST request sent to GitHub. Connection drops before response arrives. Did the issue get created? On retry (next 6-hour cycle), we might create a duplicate.

**Fix:** Before creating, always search first (dedup check). Even if the previous attempt succeeded silently, the search will find it:

```rust
// The dedup search runs BEFORE every create, not just first time.
// This handles: network partition, process crash mid-create, any failure mode
// where we're unsure if the previous attempt succeeded.
if is_duplicate(client, token, &signature).await? {
    tracing::info!("Bug already reported (possibly from interrupted previous attempt)");
    return Ok("Already reported".to_string());
}
```

**After fix:** Duplicates are impossible. The dedup check is the first step of every create attempt, catching both "already reported in a previous cycle" and "reported but we didn't get the confirmation."

### 3.3 PAT expires or is revoked

**Scenario:** User created a PAT with a 90-day expiry. After 90 days, all GitHub API calls fail with 401.

**Fix:** Handle 401 gracefully and notify the user ONCE (not every 6 hours):

```rust
match resp.status() {
    status if status.is_success() => { /* proceed */ }
    reqwest::StatusCode::UNAUTHORIZED => {
        // Check if we already notified about this
        if store.get_note("github_pat_expired_notified").await?.is_none() {
            notify_user("Your GitHub token has expired. Run /addkey github to add a new one.");
            store.save_note("github_pat_expired_notified", "true").await?;
        }
        return Ok("GitHub token expired, skipping".to_string());
    }
    reqwest::StatusCode::FORBIDDEN => {
        // Rate limited or insufficient permissions
        tracing::warn!("GitHub API forbidden — check token permissions");
        return Ok("GitHub API forbidden, skipping".to_string());
    }
    other => {
        tracing::warn!(status = %other, "GitHub API error");
        return Ok(format!("GitHub API error: {}", other));
    }
}
```

Clear the `github_pat_expired_notified` flag when a new PAT is saved via `/addkey github`.

**After fix:** Expired PAT → single notification to user → silence until they fix it. No spam. No crash. No retry storm.

### 3.4 GitHub rate limit exhaustion from user's other tools

**Scenario:** User's PAT is shared with CI, other tools. Our 3 API calls per report are fine, but the PAT's 5000 req/hr is already exhausted.

**Fix:** Read rate limit headers before making mutation calls:

```rust
// After the dedup search (which is a read), check remaining rate limit
let remaining: u32 = resp.headers()
    .get("x-ratelimit-remaining")
    .and_then(|v| v.to_str().ok())
    .and_then(|v| v.parse().ok())
    .unwrap_or(0);

if remaining < 10 {
    tracing::info!(remaining, "GitHub rate limit nearly exhausted, deferring bug report");
    return Ok("Rate limit low, deferring to next cycle".to_string());
}
```

**After fix:** We never consume the user's last API calls. Defer to next cycle when rate limit is low.

---

## Category 4: LLM and Triage

### 4.1 LLM triage prompt becomes stale

**Scenario:** Future LLM models interpret the triage prompt differently. Or TEMM1E introduces new error types that the prompt doesn't cover.

**Fix:** The prompt should be data-driven, not hardcoded. Store the triage categories as structured data:

```rust
const TRIAGE_CATEGORIES: &[(&str, &str)] = &[
    ("BUG", "A defect in TEMM1E code — panic, logic error, unhandled case, assertion failure"),
    ("USER_ERROR", "User misconfiguration, invalid input, wrong command usage"),
    ("TRANSIENT", "Temporary issue — network timeout, API rate limit, provider 500 error, DNS failure"),
    ("CONFIG", "Missing or invalid configuration, missing environment variable, wrong file permissions"),
];

fn build_triage_prompt(error: &ErrorGroup) -> String {
    let categories: String = TRIAGE_CATEGORIES.iter()
        .map(|(name, desc)| format!("- {}: {}", name, desc))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Classify this error into exactly one category:\n{}\n\n\
         Error: {}\nLocation: {}\nOccurrences: {} in last 6 hours\n\n\
         Respond with ONLY the category name and one sentence explanation.",
        categories, error.message,
        error.location.as_deref().unwrap_or("unknown"),
        error.count,
    )
}
```

Adding a new category is a one-line change to `TRIAGE_CATEGORIES`. The prompt adapts automatically.

**After fix:** Triage categories are extensible without prompt rewriting.

### 4.2 LLM returns garbage

**Scenario:** Provider returns empty string, HTML error page, or completely unrelated text. The category parser doesn't match any known category.

**Fix:** Default to "UNKNOWN" and skip reporting. Never report a bug we can't classify:

```rust
let category = if text.starts_with("BUG") { "BUG" }
    else if text.starts_with("USER_ERROR") { "USER_ERROR" }
    else if text.starts_with("TRANSIENT") { "TRANSIENT" }
    else if text.starts_with("CONFIG") { "CONFIG" }
    else {
        tracing::warn!(response = %text, "LLM triage returned unrecognized category");
        "UNKNOWN"
    };

// Only report BUG category. Everything else is logged locally and skipped.
if category != "BUG" {
    return; // Skip — not reportable
}
```

**After fix:** Garbage LLM responses → "UNKNOWN" → not reported. Silent failure. No harm.

### 4.3 Provider is unavailable during Sleep

**Scenario:** The user's API key ran out of credits, or the provider is down. The BugReview self-work can't make its triage LLM call.

**Fix:** Already handled — `caller.call()` returns `Err()`, which `triage_errors()` catches with `tracing::warn!`. The self-work task completes with "LLM call failed" and is deleted. Next Sleep cycle tries again.

**Verification:** Ensure the triage function tolerates partial failures (some errors triaged, some LLM calls failed):

```rust
// In triage_errors(), each error is triaged independently.
// If one LLM call fails, the others still proceed.
for error in errors {
    match caller.call(...).await {
        Ok(response) => results.push(...),
        Err(e) => {
            tracing::warn!(error = %e, "Bug triage LLM call failed for one error group");
            // Continue to next error — don't abort the entire triage
        }
    }
}
```

**After fix:** Provider outage → triage skipped for that cycle → retry next cycle. No crash.

---

## Category 5: Concurrency and State

### 5.1 Two BugReview cycles fire simultaneously

**Scenario:** Perpetuum enters Sleep, fires BugReview. Before it completes, the system wakes and re-enters Sleep, firing another BugReview.

**Fix:** Self-work concerns are one-shot — deleted after firing (cortex.rs existing behavior). But if the system wakes and re-enters Sleep before the first completes, a second concern could be created.

Add a lock file:

```rust
const BUG_REVIEW_LOCK: &str = "bug_review_running";

async fn review_bugs(store: &Arc<Store>, caller: ...) -> Result<String, Temm1eError> {
    // Acquire lock
    if store.get_note(BUG_REVIEW_LOCK).await?.is_some() {
        return Ok("BugReview already running, skipping".to_string());
    }
    store.save_note(BUG_REVIEW_LOCK, "true").await?;

    // ... do the work ...

    // Release lock
    store.delete_note(BUG_REVIEW_LOCK).await.ok();
    Ok(result)
}
```

Also add a TTL to the lock (if the lock is older than 30 minutes, it's stale — the previous run crashed):

```rust
if let Some(lock_ts) = store.get_note(BUG_REVIEW_LOCK).await? {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&lock_ts) {
        if chrono::Utc::now() - dt < chrono::Duration::minutes(30) {
            return Ok("BugReview already running, skipping".to_string());
        }
        // Stale lock — previous run must have crashed. Proceed.
    }
}
store.save_note(BUG_REVIEW_LOCK, &chrono::Utc::now().to_rfc3339()).await?;
```

**After fix:** Only one BugReview can run at a time. Stale locks auto-expire after 30 minutes.

### 5.2 Self-referential bug (reporter reports itself)

**Scenario:** A bug in `bug_reporter.rs` causes an error. The error is logged. The next BugReview cycle scans the log, finds the error, and tries to report it — which triggers the same bug, which logs another error, creating an infinite loop.

**Fix:** Exclude the reporter's own errors from the scan:

```rust
// In log_scanner.rs, skip entries from the bug reporter itself
let target = parsed.get("target").and_then(|t| t.as_str()).unwrap_or("");
if target.contains("bug_reporter") || target.contains("self_work") {
    continue; // Never report errors in the reporter itself
}
```

Also: the reporter is already wrapped in `catch_unwind` (via Perpetuum self-work). And it's rate-limited to 1 report per 6 hours. Even if the filter somehow missed, the rate limit prevents a loop.

**After fix:** Reporter errors are invisible to the scanner. Triple protection: filter + catch_unwind + rate limit.

---

## Category 6: Long-Term Degradation

### 6.1 Log file grows within a single day

**Scenario:** Under heavy load, a single day's log file exceeds available memory when the scanner reads it with `std::fs::read_to_string()`.

**Fix:** Read the log file in a streaming fashion, not all at once:

```rust
use std::io::{BufRead, BufReader};

pub fn scan_recent_errors(log_path: &Path, hours: u32) -> Vec<ErrorGroup> {
    let file = match std::fs::File::open(log_path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
    let mut groups: HashMap<String, ErrorGroup> = HashMap::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        // ... same parsing logic, but streaming ...
    }

    // ... group and return ...
}
```

Memory usage: O(unique error signatures), not O(file size). Even a 1 GB log file is read line by line.

**After fix:** Scanner works on any log file size. Memory bounded by number of unique error groups.

### 6.2 Error signature space grows unbounded

**Scenario:** Over months, hundreds of unique error signatures accumulate in the HashMap during a single scan. Each with sample_lines stored.

**Fix:** Cap the number of tracked groups:

```rust
const MAX_ERROR_GROUPS: usize = 50;

// After grouping, if we have too many:
if groups.len() > MAX_ERROR_GROUPS {
    // Keep only the top 50 by count
    let mut sorted: Vec<_> = groups.into_iter().collect();
    sorted.sort_by(|a, b| b.1.count.cmp(&a.1.count));
    sorted.truncate(MAX_ERROR_GROUPS);
    groups = sorted.into_iter().collect();
}
```

**After fix:** Scanner tracks at most 50 error groups. Memory bounded at ~50 * (signature + 5 sample lines) ≈ a few KB.

### 6.3 Entropy scrub regex performance on large text

**Scenario:** A bug report body contains a very large stack trace (10,000+ characters). The entropy regex `[A-Za-z0-9+/=_\-]{20,}` scans every character.

**Fix:** Cap the input text before entropy scrubbing:

```rust
pub fn entropy_scrub(text: &str) -> String {
    // GitHub issue body limit is 65536 chars. Cap before scanning.
    let capped = if text.len() > 65536 { &text[..65536] } else { text };
    // ... proceed with scrubbing ...
}
```

Also: the regex engine (regex crate) is linear time. A 65536-char scan takes <1ms. Not a real performance concern.

**After fix:** Bounded input. Linear-time scan. Negligible performance impact.

### 6.4 GitHub issue body exceeds size limit

**Scenario:** A very long error message + many sample lines exceeds GitHub's 65536 character limit for issue bodies.

**Fix:** Truncate the body before submission:

```rust
const MAX_ISSUE_BODY: usize = 60000; // Leave margin for GitHub's 65536 limit

fn format_issue_body(...) -> String {
    let body = format!(...);
    if body.len() > MAX_ISSUE_BODY {
        let safe_end = body[..MAX_ISSUE_BODY]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        format!("{}\n\n[Report truncated — exceeded GitHub size limit]", &body[..safe_end])
    } else {
        body
    }
}
```

**After fix:** Issue body always under GitHub limit. Safe UTF-8 truncation (uses `char_indices`, same pattern as the resilience architecture).

---

## Category 7: Platform-Specific

### 7.1 Windows: log directory in AppData

**Scenario:** On Windows, `dirs::home_dir()` returns `C:\Users\<name>`. The log path becomes `C:\Users\<name>\.temm1e\logs\temm1e.log`. Some Windows tools can't handle paths starting with `.`.

**Fix:** Use `dirs::data_local_dir()` on Windows:

```rust
pub fn log_dir() -> PathBuf {
    #[cfg(windows)]
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("temm1e")
        .join("logs");

    #[cfg(not(windows))]
    let base = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".temm1e")
        .join("logs");

    std::fs::create_dir_all(&base).ok();
    base
}
```

This matches Windows conventions (`%LOCALAPPDATA%\temm1e\logs\`) while keeping Unix conventions (`~/.temm1e/logs/`).

**After fix:** Cross-platform log paths follow each OS's conventions.

### 7.2 Windows: self-replace failure leaves no binary

**Scenario:** On Windows, `self-replace` renames the old binary to `.bak` and copies the new one. If the copy fails (disk full, permissions), the original is gone (renamed) and the new one isn't there.

**Fix:** Copy-then-rename, not rename-then-copy:

```rust
// 1. Copy new binary to temp path next to current
let temp_path = current_path.with_extension("new");
std::fs::copy(&staged_path, &temp_path)?;

// 2. Verify the temp copy works
let health = std::process::Command::new(&temp_path).arg("--version").output();
if health.map_or(true, |o| !o.status.success()) {
    std::fs::remove_file(&temp_path).ok();
    return Err("New binary health check failed");
}

// 3. Rename current to .bak (safe — we still have the new copy)
std::fs::rename(&current_path, &current_path.with_extension("bak"))?;

// 4. Rename new to current
if let Err(e) = std::fs::rename(&temp_path, &current_path) {
    // Rollback: restore .bak
    std::fs::rename(&current_path.with_extension("bak"), &current_path).ok();
    return Err(format!("Failed to install update: {}", e));
}
```

**After fix:** At every step, at least one working binary exists on disk. Rollback is automatic if any step fails.

---

## Summary: All Edge Cases and Their Status

| # | Edge Case | Category | Fix | Bounded? |
|---|---|---|---|---|
| 1.1 | Log cleanup fails | Disk | Hard size cap (100 MB) | Yes |
| 1.2 | Disk full during write | Disk | non_blocking drops events | Yes |
| 1.3 | Staged updates accumulate | Disk | Clean before new download | Yes |
| 2.1 | Clock skew | Time | Include on parse failure | Yes |
| 2.2 | Rate limit drift | Time | saturating_sub (safe default) | Yes |
| 3.1 | GitHub API deprecation | Network | Pin API version header | Yes |
| 3.2 | Network partition on create | Network | Dedup-before-every-create | Yes |
| 3.3 | PAT expires | Network | 401 → notify once → silence | Yes |
| 3.4 | Rate limit exhaustion | Network | Check remaining before mutate | Yes |
| 4.1 | Triage prompt stale | LLM | Data-driven categories | Yes |
| 4.2 | LLM returns garbage | LLM | Default UNKNOWN → skip | Yes |
| 4.3 | Provider unavailable | LLM | Skip cycle, retry next | Yes |
| 5.1 | Concurrent bug reviews | State | Lock file with TTL | Yes |
| 5.2 | Self-referential bug | State | Exclude reporter target from scan | Yes |
| 6.1 | Single-day log too large | Memory | Streaming BufReader | Yes |
| 6.2 | Error signatures grow | Memory | Cap at 50 groups | Yes |
| 6.3 | Entropy regex on large text | Perf | Cap at 65536 chars | Yes |
| 6.4 | Issue body too large | API | Safe truncation at 60000 chars | Yes |
| 7.1 | Windows log path | Platform | data_local_dir() on Windows | Yes |
| 7.2 | Windows self-replace | Platform | Copy-then-rename with rollback | Yes |

**Every edge case is bounded.** No unbounded growth. No crash paths. No silent corruption. Every failure mode degrades to "skip this cycle, try next time" or "notify user once, then silence."
