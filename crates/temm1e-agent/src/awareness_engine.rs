//! Tem Aware — consciousness observation engine.
//!
//! Two-tier observation: rule-based triggers (free, no LLM call) run first.
//! If no rule triggers and the turn warrants deep observation, an LLM call
//! analyzes the TurnObservation and produces an intervention.

use crate::awareness::{AwarenessConfig, ConsciousnessIntervention, TurnObservation};
use std::sync::Mutex;
use tracing;

/// Destructive tool patterns that warrant an Override intervention.
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -r",
    "rmdir",
    "drop table",
    "drop database",
    "truncate",
    "format",
    "del /f",
    "deltree",
];

/// The consciousness engine observes TurnObservations and produces interventions.
///
/// Uses interior mutability (Mutex) for session state so it can be called
/// from process_message() which takes `&self`.
pub struct AwarenessEngine {
    config: AwarenessConfig,
    session_notes: Mutex<Vec<String>>,
    intervention_count: Mutex<u32>,
    /// The pending consciousness note to inject into the next turn.
    /// Set after observe(), consumed at the start of the next process_message().
    pending_note: Mutex<Option<String>>,
}

impl AwarenessEngine {
    /// Create a new consciousness engine with the given config.
    pub fn new(config: AwarenessConfig) -> Self {
        tracing::info!(
            enabled = config.enabled,
            mode = %config.observation_mode,
            threshold = config.confidence_threshold,
            max_interventions = config.max_interventions_per_session,
            "Tem Aware consciousness engine initialized"
        );
        Self {
            config,
            session_notes: Mutex::new(Vec::new()),
            intervention_count: Mutex::new(0),
            pending_note: Mutex::new(None),
        }
    }

    /// Check if consciousness observation is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Take the pending consciousness note (if any) for injection into the
    /// next turn's system prompt. Clears the note after taking.
    pub fn take_pending_note(&self) -> Option<String> {
        self.pending_note.lock().ok().and_then(|mut n| n.take())
    }

    /// Observe a completed turn and decide whether to intervene.
    ///
    /// This is called AFTER process_message() completes. If an intervention
    /// is produced, the note is stored in `pending_note` for injection into
    /// the next turn.
    pub fn observe(&self, observation: &TurnObservation) -> ConsciousnessIntervention {
        if !self.config.enabled {
            return ConsciousnessIntervention::NoAction;
        }

        // Check max interventions
        let count = self.intervention_count.lock().map(|c| *c).unwrap_or(0);
        if count >= self.config.max_interventions_per_session {
            tracing::debug!(
                count,
                max = self.config.max_interventions_per_session,
                "Consciousness: max interventions reached, going quiet"
            );
            return ConsciousnessIntervention::NoAction;
        }

        // Tier 1: Rule-based triggers (free, no LLM call)
        let intervention = self.check_rules(observation);

        // If rules triggered an intervention, store it
        if !matches!(intervention, ConsciousnessIntervention::NoAction) {
            self.record_intervention(&intervention, observation);
            return intervention;
        }

        // Tier 2: LLM-based deep observation (costs tokens)
        // Only if observation_mode is not "rules_only"
        if self.config.observation_mode == "rules_only" {
            return ConsciousnessIntervention::NoAction;
        }

        // For now, we only implement rule-based triggers.
        // LLM-based observation will be added in a future iteration
        // after the rule-based system is validated.
        ConsciousnessIntervention::NoAction
    }

    /// Rule-based triggers — zero LLM cost.
    fn check_rules(&self, obs: &TurnObservation) -> ConsciousnessIntervention {
        // Rule 1: Consecutive tool failures
        if obs.max_consecutive_failures >= 3 {
            let failing_tools: Vec<&str> = obs
                .tool_results
                .iter()
                .filter(|r| *r != "success")
                .map(|r| r.as_str())
                .collect();
            let tool_info = if failing_tools.is_empty() {
                "multiple tools".to_string()
            } else {
                failing_tools[0].to_string()
            };
            return ConsciousnessIntervention::Whisper(format!(
                "Awareness: {} consecutive tool failures detected. \
                 The current approach may not be working. \
                 Consider a completely different strategy rather than retrying. \
                 Failed tool context: {}",
                obs.max_consecutive_failures, tool_info
            ));
        }

        // Rule 2: Budget warning
        if obs.budget_limit_usd > 0.0 {
            let percent_used = (obs.cumulative_cost_usd / obs.budget_limit_usd) * 100.0;
            if percent_used > 80.0 {
                return ConsciousnessIntervention::Whisper(format!(
                    "Awareness: Budget is at {:.0}% (${:.4} of ${:.2} limit). \
                     Prioritize completing the most important remaining work \
                     with minimal additional API calls.",
                    percent_used, obs.cumulative_cost_usd, obs.budget_limit_usd
                ));
            }
        }

        // Rule 3: Destructive tool detection
        for tool_result in &obs.tool_results {
            let lower = tool_result.to_lowercase();
            for pattern in DESTRUCTIVE_PATTERNS {
                if lower.contains(pattern) {
                    return ConsciousnessIntervention::Whisper(format!(
                        "Awareness: A potentially destructive operation was detected \
                         ('{}' pattern in tool output). Verify this action was \
                         explicitly requested by the user before proceeding with \
                         similar operations.",
                        pattern
                    ));
                }
            }
        }

        // Rule 4: Strategy rotation suggests the agent is stuck
        if obs.strategy_rotations >= 2 {
            return ConsciousnessIntervention::Whisper(
                "Awareness: Multiple strategy rotations have occurred, suggesting \
                 the agent is stuck in a loop. Consider asking the user for \
                 clarification or trying a fundamentally different approach."
                    .to_string(),
            );
        }

        // Rule 5: Long conversation without progress
        if obs.turn_number > 8 && obs.tools_called.is_empty() && obs.category == "Order" {
            return ConsciousnessIntervention::Whisper(
                "Awareness: This is turn {} of a task-oriented conversation, \
                 but no tools were used this turn. If the task requires action, \
                 consider executing rather than discussing."
                    .replace("{}", &obs.turn_number.to_string()),
            );
        }

        ConsciousnessIntervention::NoAction
    }

    /// Record an intervention: store the note, increment counter.
    fn record_intervention(&self, intervention: &ConsciousnessIntervention, obs: &TurnObservation) {
        let note = match intervention {
            ConsciousnessIntervention::Whisper(text) => text.clone(),
            ConsciousnessIntervention::Redirect { memory_query } => {
                format!("Consciousness recalled memory: {}", memory_query)
            }
            ConsciousnessIntervention::Override {
                block_tool, reason, ..
            } => {
                format!("Consciousness blocked tool '{}': {}", block_tool, reason)
            }
            ConsciousnessIntervention::NoAction => return,
        };

        tracing::info!(
            turn = obs.turn_number,
            intervention = %note,
            "Tem Aware: consciousness intervention"
        );

        // Store as pending note for next turn injection
        if let Ok(mut pending) = self.pending_note.lock() {
            *pending = Some(note.clone());
        }

        // Add to session notes history
        if let Ok(mut notes) = self.session_notes.lock() {
            notes.push(format!("Turn {}: {}", obs.turn_number, note));
        }

        // Increment counter
        if let Ok(mut count) = self.intervention_count.lock() {
            *count += 1;
        }
    }

    /// Get all session notes (for inclusion in future observations).
    pub fn session_notes(&self) -> Vec<String> {
        self.session_notes
            .lock()
            .map(|n| n.clone())
            .unwrap_or_default()
    }

    /// Reset session state (for new conversations).
    pub fn reset_session(&self) {
        if let Ok(mut notes) = self.session_notes.lock() {
            notes.clear();
        }
        if let Ok(mut count) = self.intervention_count.lock() {
            *count = 0;
        }
        if let Ok(mut pending) = self.pending_note.lock() {
            *pending = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> AwarenessConfig {
        AwarenessConfig {
            enabled: true,
            ..Default::default()
        }
    }

    fn make_observation() -> TurnObservation {
        TurnObservation {
            turn_number: 1,
            session_id: "test".into(),
            user_message_preview: "do something".into(),
            category: "Order".into(),
            difficulty: "Standard".into(),
            model_used: "test-model".into(),
            input_tokens: 500,
            output_tokens: 100,
            cost_usd: 0.001,
            cumulative_cost_usd: 0.001,
            budget_limit_usd: 0.0,
            tools_called: vec!["shell".into()],
            tool_results: vec!["success".into()],
            max_consecutive_failures: 0,
            strategy_rotations: 0,
            response_preview: "done".into(),
            circuit_breaker_state: "closed".into(),
            previous_notes: vec![],
        }
    }

    #[test]
    fn test_no_action_on_normal_turn() {
        let engine = AwarenessEngine::new(make_config());
        let obs = make_observation();
        let result = engine.observe(&obs);
        assert!(matches!(result, ConsciousnessIntervention::NoAction));
    }

    #[test]
    fn test_disabled_returns_no_action() {
        let config = AwarenessConfig::default(); // enabled = false
        let engine = AwarenessEngine::new(config);
        let mut obs = make_observation();
        obs.max_consecutive_failures = 10; // Would normally trigger
        let result = engine.observe(&obs);
        assert!(matches!(result, ConsciousnessIntervention::NoAction));
    }

    #[test]
    fn test_consecutive_failures_trigger() {
        let engine = AwarenessEngine::new(make_config());
        let mut obs = make_observation();
        obs.max_consecutive_failures = 3;
        obs.tool_results = vec!["error: permission denied".into()];
        let result = engine.observe(&obs);
        assert!(
            matches!(result, ConsciousnessIntervention::Whisper(ref s) if s.contains("consecutive tool failures")),
            "Expected whisper about failures, got: {:?}",
            result
        );
    }

    #[test]
    fn test_budget_warning_trigger() {
        let engine = AwarenessEngine::new(make_config());
        let mut obs = make_observation();
        obs.budget_limit_usd = 1.0;
        obs.cumulative_cost_usd = 0.85; // 85%
        let result = engine.observe(&obs);
        assert!(
            matches!(result, ConsciousnessIntervention::Whisper(ref s) if s.contains("Budget")),
            "Expected budget whisper, got: {:?}",
            result
        );
    }

    #[test]
    fn test_budget_no_trigger_when_unlimited() {
        let engine = AwarenessEngine::new(make_config());
        let mut obs = make_observation();
        obs.budget_limit_usd = 0.0; // Unlimited
        obs.cumulative_cost_usd = 100.0;
        let result = engine.observe(&obs);
        assert!(matches!(result, ConsciousnessIntervention::NoAction));
    }

    #[test]
    fn test_destructive_pattern_trigger() {
        let engine = AwarenessEngine::new(make_config());
        let mut obs = make_observation();
        obs.tool_results = vec!["executed: rm -rf /tmp/test".into()];
        let result = engine.observe(&obs);
        assert!(
            matches!(result, ConsciousnessIntervention::Whisper(ref s) if s.contains("destructive")),
            "Expected destructive whisper, got: {:?}",
            result
        );
    }

    #[test]
    fn test_strategy_rotation_trigger() {
        let engine = AwarenessEngine::new(make_config());
        let mut obs = make_observation();
        obs.strategy_rotations = 2;
        let result = engine.observe(&obs);
        assert!(
            matches!(result, ConsciousnessIntervention::Whisper(ref s) if s.contains("strategy rotations")),
            "Expected rotation whisper, got: {:?}",
            result
        );
    }

    #[test]
    fn test_max_interventions_limit() {
        let mut config = make_config();
        config.max_interventions_per_session = 2;
        let engine = AwarenessEngine::new(config);

        let mut obs = make_observation();
        obs.max_consecutive_failures = 3;

        // First two interventions should work
        let r1 = engine.observe(&obs);
        assert!(matches!(r1, ConsciousnessIntervention::Whisper(_)));
        let r2 = engine.observe(&obs);
        assert!(matches!(r2, ConsciousnessIntervention::Whisper(_)));

        // Third should be suppressed
        let r3 = engine.observe(&obs);
        assert!(matches!(r3, ConsciousnessIntervention::NoAction));
    }

    #[test]
    fn test_pending_note_lifecycle() {
        let engine = AwarenessEngine::new(make_config());
        let mut obs = make_observation();
        obs.max_consecutive_failures = 3;

        // Observe → produces whisper → stores pending note
        let _ = engine.observe(&obs);
        assert!(engine.take_pending_note().is_some());

        // After taking, it should be None
        assert!(engine.take_pending_note().is_none());
    }

    #[test]
    fn test_session_notes_accumulate() {
        let engine = AwarenessEngine::new(make_config());
        let mut obs = make_observation();
        obs.max_consecutive_failures = 3;

        engine.observe(&obs);
        obs.turn_number = 2;
        engine.observe(&obs);

        let notes = engine.session_notes();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("Turn 1"));
        assert!(notes[1].contains("Turn 2"));
    }

    #[test]
    fn test_reset_session() {
        let engine = AwarenessEngine::new(make_config());
        let mut obs = make_observation();
        obs.max_consecutive_failures = 3;
        engine.observe(&obs);

        assert!(!engine.session_notes().is_empty());
        engine.reset_session();
        assert!(engine.session_notes().is_empty());
        assert!(engine.take_pending_note().is_none());
    }

    #[test]
    fn test_rules_only_mode() {
        let mut config = make_config();
        config.observation_mode = "rules_only".into();
        let engine = AwarenessEngine::new(config);
        let obs = make_observation(); // Normal turn
        let result = engine.observe(&obs);
        assert!(matches!(result, ConsciousnessIntervention::NoAction));
    }
}
