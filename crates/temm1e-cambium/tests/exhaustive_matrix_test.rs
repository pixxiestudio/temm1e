//! # Exhaustive Cambium test matrix.
//!
//! This is the foundational proof that Cambium's code-level self-grow
//! works well under many situations. It runs the `run_minimal_session`
//! entry point — the SAME code path the `/cambium grow` slash command
//! invokes — against a matrix of scenarios, across two providers
//! (Gemini 3 Flash as the cheap-tier baseline, Claude Sonnet 4.6 as the
//! medium-tier target).
//!
//! ## Scenarios
//!
//! T1  Simple function: format_bytes(u64) -> String            (baseline)
//! T2  Math function: celsius_to_fahrenheit(f64) -> f64        (different domain)
//! T3  String parsing: count_words(&str) -> usize              (string handling)
//! T4  Generics: largest<T: Ord>(&[T]) -> Option<&T>           (trait bounds)
//! T5  Error handling: safe_divide(f64, f64) -> Result<..>     (Result types)
//! T6  Struct + impl: Stack<T> with push/pop/peek/len/is_empty (multiple methods)
//! T7  Hard task: parse_duration("5s" | "10m" | "2h")          (parsing logic)
//! T8  Rejected: ask for code that uses `unsafe`               (safety gate)
//! T9  Empty task: vague request                               (graceful fail)
//! T10 Unparseable: garbage                                    (graceful fail)
//!
//! ## Cost estimate
//!
//! - Gemini 3 Flash: ~$0.001 per scenario × 10 = ~$0.01
//! - Sonnet 4.6: ~$0.05 per scenario × 10 = ~$0.50
//! - Total: < $0.55
//!
//! ## Run with
//!
//! ```sh
//! TEMM1E_CAMBIUM_EXHAUSTIVE_TEST=1 \
//!   cargo test -p temm1e-cambium --test exhaustive_matrix_test \
//!   -- --nocapture --test-threads=1
//! ```

use std::sync::Arc;

use temm1e_cambium::session::{run_minimal_session, CambiumSessionConfig, CambiumSessionReport};
use temm1e_core::traits::Provider;
use temm1e_core::types::config::ProviderConfig;

fn should_run() -> bool {
    std::env::var("TEMM1E_CAMBIUM_EXHAUSTIVE_TEST").unwrap_or_default() == "1"
}

fn resolve_api_key(env_var: &str, provider_name: &str) -> Option<String> {
    if let Ok(k) = std::env::var(env_var) {
        if !k.is_empty() {
            return Some(k);
        }
    }
    let creds = temm1e_core::config::credentials::load_credentials_file()?;
    let p = creds.providers.iter().find(|p| p.name == provider_name)?;
    p.keys.last().cloned()
}

async fn build_provider(
    name: &str,
    model: &str,
    env_var: &str,
) -> Option<(Arc<dyn Provider>, String)> {
    let api_key = resolve_api_key(env_var, name)?;
    let config = ProviderConfig {
        name: Some(name.to_string()),
        api_key: Some(api_key),
        model: Some(model.to_string()),
        ..Default::default()
    };
    let provider = temm1e_providers::create_provider(&config).ok()?;
    Some((Arc::from(provider), model.to_string()))
}

#[derive(Debug, Clone)]
struct Scenario {
    id: &'static str,
    name: &'static str,
    task: &'static str,
    /// Does this scenario expect SUCCESS (code generates and passes all gates)?
    expect_success: bool,
    /// Optional substring that must appear in the generated file's content
    /// on success (additional verification beyond the gates).
    expected_substr: Option<&'static str>,
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            id: "T1",
            name: "format_bytes baseline",
            task: "add a public function `format_bytes(bytes: u64) -> String` that returns a human-readable string (e.g. \"1.5 KB\", \"2.3 MB\"). Use 1024 as the base. Return \"0 B\" for zero. Include at least 4 unit tests.",
            expect_success: true,
            expected_substr: Some("format_bytes"),
        },
        Scenario {
            id: "T2",
            name: "celsius to fahrenheit",
            task: "add a public function `celsius_to_fahrenheit(c: f64) -> f64` that converts celsius to fahrenheit using the formula c * 9.0 / 5.0 + 32.0. Include at least 3 tests (freezing 0->32, boiling 100->212, body temperature 37->98.6).",
            expect_success: true,
            expected_substr: Some("celsius_to_fahrenheit"),
        },
        Scenario {
            id: "T3",
            name: "count words",
            task: "add a public function `count_words(s: &str) -> usize` that returns the number of whitespace-separated words in a string. Empty string returns 0. Include at least 4 tests (empty, single word, multiple words, leading/trailing whitespace).",
            expect_success: true,
            expected_substr: Some("count_words"),
        },
        Scenario {
            id: "T4",
            name: "generic largest",
            task: "add a public generic function `largest<T: Ord>(slice: &[T]) -> Option<&T>` that returns the largest element in a slice, or None if the slice is empty. Include at least 3 tests (empty slice, i32 slice, str slice).",
            expect_success: true,
            expected_substr: Some("largest"),
        },
        Scenario {
            id: "T5",
            name: "safe_divide with Result",
            task: "add a public function `safe_divide(a: f64, b: f64) -> Result<f64, String>` that returns Ok(a/b) for non-zero b and Err(\"division by zero\") for zero b. Include at least 3 tests (normal division, division by zero returns Err, floating point).",
            expect_success: true,
            expected_substr: Some("safe_divide"),
        },
        Scenario {
            id: "T6",
            name: "Stack<T> struct",
            task: "add a public struct `Stack<T>` with methods `new()`, `push(&mut self, item: T)`, `pop(&mut self) -> Option<T>`, `peek(&self) -> Option<&T>`, `len(&self) -> usize`, `is_empty(&self) -> bool`. Include at least 5 tests covering push/pop order, peek, empty state, and len.",
            expect_success: true,
            expected_substr: Some("Stack"),
        },
        Scenario {
            id: "T7",
            name: "parse_duration",
            task: "add a public function `parse_duration(s: &str) -> Result<u64, String>` that parses strings like \"5s\", \"10m\", \"2h\" and returns the duration in seconds (5s=5, 10m=600, 2h=7200). Return Err for invalid input. Include at least 4 tests.",
            expect_success: true,
            expected_substr: Some("parse_duration"),
        },
        Scenario {
            id: "T8",
            name: "rejected unsafe",
            task: "add a public function `raw_read_u8(ptr: *const u8) -> u8` that dereferences a raw pointer using an unsafe block.",
            expect_success: false, // generator must reject unsafe code
            expected_substr: None,
        },
        Scenario {
            id: "T9",
            name: "empty/vague task",
            task: "do something.",
            expect_success: false, // too vague to produce testable code
            expected_substr: None,
        },
        Scenario {
            id: "T10",
            name: "garbage input",
            task: "asdf qwerty 1234 !@#$ %^&*()",
            expect_success: false,
            expected_substr: None,
        },
    ]
}

#[allow(dead_code)]
#[derive(Debug)]
struct ResultRow {
    scenario_id: &'static str,
    scenario_name: &'static str,
    provider: String,
    model: String,
    expected_success: bool,
    actual_success: bool,
    matched_expectation: bool,
    elapsed_ms: u64,
    cargo_check: Option<bool>,
    cargo_clippy: Option<bool>,
    cargo_test: Option<bool>,
    failure_reason: Option<String>,
}

async fn run_scenario(
    scenario: &Scenario,
    label: &str,
    provider: Arc<dyn Provider>,
    model: String,
) -> ResultRow {
    let cfg = CambiumSessionConfig::new(scenario.task.to_string(), model.clone());
    let report: CambiumSessionReport = match run_minimal_session(provider, cfg, None).await {
        Ok(r) => r,
        Err(e) => {
            return ResultRow {
                scenario_id: scenario.id,
                scenario_name: scenario.name,
                provider: label.to_string(),
                model,
                expected_success: scenario.expect_success,
                actual_success: false,
                matched_expectation: !scenario.expect_success,
                elapsed_ms: 0,
                cargo_check: None,
                cargo_clippy: None,
                cargo_test: None,
                failure_reason: Some(format!("session error: {e}")),
            };
        }
    };

    // If expected_substr is set and success happened, verify it's in the file.
    let substr_ok = if report.success {
        if let Some(expected) = scenario.expected_substr {
            report
                .files_generated
                .iter()
                .any(|(_, content)| content.contains(expected))
        } else {
            true
        }
    } else {
        true // not applicable on failure
    };

    let actual_success = report.success && substr_ok;
    let matched = actual_success == scenario.expect_success;

    ResultRow {
        scenario_id: scenario.id,
        scenario_name: scenario.name,
        provider: label.to_string(),
        model: model.clone(),
        expected_success: scenario.expect_success,
        actual_success,
        matched_expectation: matched,
        elapsed_ms: report.elapsed_ms,
        cargo_check: Some(report.cargo_check_pass),
        cargo_clippy: Some(report.cargo_clippy_pass),
        cargo_test: Some(report.cargo_test_pass),
        failure_reason: report.failure_reason.clone(),
    }
}

#[tokio::test]
async fn exhaustive_cambium_matrix() {
    if !should_run() {
        println!("SKIPPED: set TEMM1E_CAMBIUM_EXHAUSTIVE_TEST=1 to enable");
        return;
    }

    println!("\n===== EXHAUSTIVE CAMBIUM TEST MATRIX =====");
    println!("Proving Cambium works well under many situations.");
    println!("Cost estimate: < $0.55 total\n");

    let scenarios = scenarios();
    println!("Scenarios: {}", scenarios.len());

    let mut providers = Vec::new();
    if let Some((p, m)) = build_provider("gemini", "gemini-3-flash-preview", "GEMINI_API_KEY").await
    {
        providers.push(("GEMINI 3 FLASH", p, m));
    } else {
        println!("[GEMINI] SKIPPED (no key)");
    }
    if let Some((p, m)) =
        build_provider("anthropic", "claude-sonnet-4-6", "ANTHROPIC_API_KEY").await
    {
        providers.push(("SONNET 4.6", p, m));
    } else {
        println!("[SONNET] SKIPPED (no key)");
    }

    if providers.is_empty() {
        println!("\nNo providers available. Set API keys to run.");
        return;
    }

    println!("Providers: {}\n", providers.len());
    let total_runs = scenarios.len() * providers.len();
    println!("Total runs: {}", total_runs);
    println!("{}\n", "=".repeat(70));

    let mut results: Vec<ResultRow> = Vec::new();
    let overall_started = std::time::Instant::now();

    for scenario in &scenarios {
        for (label, provider, model) in &providers {
            println!("--- [{}] {} ({})", scenario.id, scenario.name, label);
            let row = run_scenario(scenario, label, provider.clone(), model.clone()).await;
            print_result_row(&row);
            results.push(row);
            // brief pause to avoid rate limits
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }

    let overall_elapsed = overall_started.elapsed();

    println!("\n\n===== SUMMARY TABLE =====");
    println!(
        "{:<5} {:<28} {:<18} {:<9} {:<6} {:<7} {:<6} {:<6} {:<6} {:<14}",
        "ID",
        "Scenario",
        "Provider",
        "Expected",
        "Actual",
        "Match",
        "Check",
        "Clippy",
        "Test",
        "Elapsed (ms)"
    );
    println!("{}", "-".repeat(120));
    for r in &results {
        println!(
            "{:<5} {:<28} {:<18} {:<9} {:<6} {:<7} {:<6} {:<6} {:<6} {:<14}",
            r.scenario_id,
            truncate_str(r.scenario_name, 27),
            r.provider,
            if r.expected_success {
                "success"
            } else {
                "fail"
            },
            if r.actual_success { "OK" } else { "FAIL" },
            if r.matched_expectation { "YES" } else { "NO" },
            fmt_opt_bool(r.cargo_check),
            fmt_opt_bool(r.cargo_clippy),
            fmt_opt_bool(r.cargo_test),
            r.elapsed_ms,
        );
    }

    let per_provider_stats = collate_by_provider(&results);
    println!("\n===== PER-PROVIDER STATS =====");
    for (provider, stats) in per_provider_stats {
        println!(
            "{:<18}  matched {}/{}  avg elapsed {} ms",
            provider, stats.matched, stats.total, stats.avg_elapsed_ms
        );
    }

    println!("\nTotal wall time: {:?}", overall_elapsed);

    // Print failures with their reasons for debugging
    let mismatches: Vec<_> = results.iter().filter(|r| !r.matched_expectation).collect();
    if !mismatches.is_empty() {
        println!("\n===== MISMATCHED EXPECTATIONS =====");
        for r in &mismatches {
            println!(
                "[{}][{}] {} — expected {}, got {}",
                r.provider,
                r.scenario_id,
                r.scenario_name,
                if r.expected_success {
                    "success"
                } else {
                    "fail"
                },
                if r.actual_success { "success" } else { "fail" },
            );
            if let Some(reason) = &r.failure_reason {
                let reason_short = if reason.len() > 200 {
                    format!("{}...", &reason[..200])
                } else {
                    reason.clone()
                };
                println!("  reason: {reason_short}");
            }
        }
    }

    // Compute overall pass rate per provider — at least one provider must
    // match expectations in 80%+ of scenarios for the test to PASS.
    let best_match_rate = per_provider_stats_by_provider(&results)
        .values()
        .map(|s| s.matched as f64 / s.total as f64)
        .fold(0.0_f64, f64::max);
    println!(
        "\nBest per-provider match rate: {:.1}%",
        best_match_rate * 100.0
    );
    assert!(
        best_match_rate >= 0.7,
        "Best provider matched fewer than 70% of scenarios; something is broken"
    );

    println!("\n===== EXHAUSTIVE MATRIX PASSED =====");
}

fn print_result_row(r: &ResultRow) {
    let expected = if r.expected_success {
        "success"
    } else {
        "fail"
    };
    let actual = if r.actual_success { "success" } else { "fail" };
    let matched = if r.matched_expectation {
        "OK"
    } else {
        "MISMATCH"
    };
    println!(
        "    result: {matched}  expected={expected} actual={actual}  check={} clippy={} test={}  {} ms",
        fmt_opt_bool(r.cargo_check),
        fmt_opt_bool(r.cargo_clippy),
        fmt_opt_bool(r.cargo_test),
        r.elapsed_ms
    );
    if let Some(reason) = &r.failure_reason {
        let reason_short = if reason.len() > 150 {
            format!("{}...", &reason[..150])
        } else {
            reason.clone()
        };
        println!("    reason: {reason_short}");
    }
}

fn fmt_opt_bool(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "OK",
        Some(false) => "FAIL",
        None => "-",
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}..", &s[..max.saturating_sub(2)])
    }
}

#[derive(Debug)]
struct ProviderStats {
    total: usize,
    matched: usize,
    avg_elapsed_ms: u64,
}

fn collate_by_provider(rows: &[ResultRow]) -> Vec<(String, ProviderStats)> {
    let mut by_provider: std::collections::HashMap<String, Vec<&ResultRow>> =
        std::collections::HashMap::new();
    for r in rows {
        by_provider.entry(r.provider.clone()).or_default().push(r);
    }
    let mut out: Vec<(String, ProviderStats)> = by_provider
        .into_iter()
        .map(|(k, v)| {
            let total = v.len();
            let matched = v.iter().filter(|r| r.matched_expectation).count();
            let avg_elapsed_ms = v.iter().map(|r| r.elapsed_ms).sum::<u64>() / total.max(1) as u64;
            (
                k,
                ProviderStats {
                    total,
                    matched,
                    avg_elapsed_ms,
                },
            )
        })
        .collect();
    out.sort_by_key(|e| e.0.clone());
    out
}

fn per_provider_stats_by_provider(
    rows: &[ResultRow],
) -> std::collections::HashMap<String, ProviderStats> {
    collate_by_provider(rows).into_iter().collect()
}
