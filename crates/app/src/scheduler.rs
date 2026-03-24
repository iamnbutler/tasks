//! Automation scheduler — evaluates cron expressions and triggers automation runs.
//!
//! The scheduler runs as a background loop that checks scheduled automations
//! every minute and triggers runs when their cron expressions match.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use croner::Cron;
use server::Server;
use tracing::{debug, error, info, warn};

use models::automation::{AutomationState, TriggerType};

/// Automation scheduler that evaluates cron triggers.
pub struct AutomationScheduler {
    server: Arc<Server>,
    /// Track next run time for each automation.
    /// Key: automation_id, Value: next scheduled run time
    next_runs: HashMap<String, DateTime<Utc>>,
    /// Track last run time for each automation to prevent double-runs.
    last_runs: HashMap<String, DateTime<Utc>>,
}

impl AutomationScheduler {
    /// Create a new scheduler attached to the server.
    pub fn new(server: Arc<Server>) -> Self {
        Self {
            server,
            next_runs: HashMap::new(),
            last_runs: HashMap::new(),
        }
    }

    /// Start the scheduler loop.
    ///
    /// Spawns a background task that ticks every 60 seconds to evaluate
    /// scheduled automations and trigger runs.
    pub fn start(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));

            // Skip the first immediate tick
            interval.tick().await;

            loop {
                interval.tick().await;
                self.tick().await;
            }
        })
    }

    /// Single tick of the scheduler — check all scheduled automations.
    async fn tick(&mut self) {
        // Check operating mode — don't run in Stop mode
        let mode = self.server.mode().await;
        if mode == server::Mode::Stop {
            debug!("scheduler: skipping tick, operating mode is Stop");
            return;
        }

        let now = Utc::now();

        // Get all active scheduled automations
        let automations = self.get_scheduled_automations().await;

        if automations.is_empty() {
            return;
        }

        debug!(
            count = automations.len(),
            "scheduler: checking scheduled automations"
        );

        for automation in automations {
            // Skip if not active
            if automation.state != AutomationState::Active {
                continue;
            }

            // Extract cron expression
            let cron_expr = match &automation.trigger {
                TriggerType::Schedule { cron } => cron,
                _ => continue, // Skip non-scheduled automations
            };

            // Check if this automation should run
            match self.should_run(&automation.id, cron_expr, now) {
                Ok(true) => {
                    info!(
                        automation_id = %automation.id,
                        automation_name = %automation.name,
                        cron = %cron_expr,
                        "scheduler: triggering scheduled automation"
                    );
                    self.trigger_run(&automation.id).await;
                }
                Ok(false) => {
                    // Not time yet
                }
                Err(e) => {
                    warn!(
                        automation_id = %automation.id,
                        cron = %cron_expr,
                        error = %e,
                        "scheduler: failed to evaluate cron expression"
                    );
                }
            }
        }
    }

    /// Get all automations with scheduled triggers.
    async fn get_scheduled_automations(&self) -> Vec<models::automation::Automation> {
        let state = self.server.state.read().await;
        state
            .automations
            .values()
            .filter(|a| matches!(a.trigger, TriggerType::Schedule { .. }))
            .cloned()
            .collect()
    }

    /// Check if an automation should run based on its cron expression.
    ///
    /// Returns true if the cron expression matches the current time and
    /// we haven't already run this automation in the current minute.
    fn should_run(
        &mut self,
        automation_id: &str,
        cron_expr: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, String> {
        // Parse the cron expression
        let cron = Cron::new(cron_expr)
            .parse()
            .map_err(|e| format!("invalid cron expression: {}", e))?;

        // Calculate next run time if we don't have one
        if !self.next_runs.contains_key(automation_id) {
            if let Ok(next) = cron.find_next_occurrence(&now, false) {
                self.next_runs.insert(automation_id.to_string(), next);
                debug!(
                    automation_id = %automation_id,
                    next_run = %next,
                    "scheduler: calculated next run time"
                );
            }
        }

        // Check if it's time to run
        if let Some(next_run) = self.next_runs.get(automation_id) {
            if now >= *next_run {
                // Safety net: if find_next_occurrence fails and next_run isn't
                // advanced, prevent re-triggering within the same calendar minute.
                if let Some(last_run) = self.last_runs.get(automation_id) {
                    // Round both to minute precision to check
                    let last_minute = last_run.format("%Y-%m-%d %H:%M").to_string();
                    let current_minute = now.format("%Y-%m-%d %H:%M").to_string();
                    if last_minute == current_minute {
                        return Ok(false);
                    }
                }

                // Calculate the next run time after this one
                if let Ok(next) = cron.find_next_occurrence(&now, false) {
                    self.next_runs.insert(automation_id.to_string(), next);
                }

                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Trigger a run for the given automation.
    async fn trigger_run(&mut self, automation_id: &str) {
        let now = Utc::now();

        // Record that we're running now to prevent double-runs
        self.last_runs.insert(automation_id.to_string(), now);

        // Create the automation run via the server
        match self.server.create_automation_run(automation_id).await {
            Ok(run) => {
                info!(
                    automation_id = %automation_id,
                    run_id = %run.id,
                    "scheduler: automation run created"
                );

                // For now, immediately complete the run since we don't have
                // actual execution logic yet. In the future, this would hand off
                // to an execution engine that creates tasks or runs workflows.
                if let Err(e) = self
                    .server
                    .complete_automation_run(&run.id, Some("Scheduled run completed".to_string()))
                    .await
                {
                    error!(
                        run_id = %run.id,
                        error = %e,
                        "scheduler: failed to complete automation run"
                    );
                }
            }
            Err(e) => {
                error!(
                    automation_id = %automation_id,
                    error = %e,
                    "scheduler: failed to create automation run"
                );

                // Record the failure but continue — don't let one automation
                // failure stop the scheduler
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cron_parsing() {
        // Test that croner can parse standard cron expressions
        let expressions = [
            "* * * * *",       // every minute
            "0 * * * *",       // every hour
            "0 0 * * *",       // every day at midnight
            "0 9 * * 1-5",     // 9am on weekdays
            "*/5 * * * *",     // every 5 minutes
            "0 0 1 * *",       // first of every month
        ];

        for expr in expressions {
            let result = Cron::new(expr).parse();
            assert!(result.is_ok(), "Failed to parse: {}", expr);
        }
    }

    #[test]
    fn test_invalid_cron() {
        // Test that invalid cron expressions are rejected
        let invalid = [
            "",
            "not a cron",
            "* * *",           // too few fields
            "60 * * * *",      // invalid minute
        ];

        for expr in invalid {
            let result = Cron::new(expr).parse();
            assert!(result.is_err(), "Should have failed: {}", expr);
        }
    }
}
