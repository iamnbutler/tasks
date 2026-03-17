//! Host memory monitoring — prevents OS lockup from container memory pressure.
//!
//! Periodically samples system memory usage and emits events / takes action
//! at configurable thresholds:
//!
//! - **Warn** (default 75%): log warning, emit `system:memory:warning`
//! - **Soft limit** (default 85%): emit `system:memory:pressure`, signal dispatch to pause
//! - **Hard limit** (default 92%): emit `system:memory:emergency`, stop newest sessions

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use sysinfo::System;
use tracing::{error, info, warn};

use events::{Actor, Event, EventBus, EventType};

/// Memory pressure level observed on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemoryPressure {
    /// Below warning threshold — all clear.
    Normal,
    /// Above warning threshold — log but don't restrict.
    Warning,
    /// Above soft limit — pause dispatching new sessions.
    Pressure,
    /// Above hard limit — emergency: stop sessions to free memory.
    Emergency,
}

/// Snapshot of host memory state.
#[derive(Debug, Clone)]
pub struct MemorySnapshot {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub used_pct: u8,
    pub pressure: MemoryPressure,
}

/// Thresholds for memory pressure levels (percentages of total RAM).
#[derive(Debug, Clone, Copy)]
pub struct MemoryThresholds {
    pub warn_pct: u8,
    pub soft_limit_pct: u8,
    pub hard_limit_pct: u8,
}

impl Default for MemoryThresholds {
    fn default() -> Self {
        Self {
            warn_pct: 75,
            soft_limit_pct: 85,
            hard_limit_pct: 92,
        }
    }
}

/// Shared state for memory-based dispatch gating.
///
/// The watchdog updates this; the dispatch loop reads it.
pub struct MemoryGate {
    /// Whether dispatch should be paused due to memory pressure.
    pub dispatch_paused: AtomicBool,
    /// Current memory usage percentage (updated by watchdog).
    pub current_pct: AtomicU8,
}

impl MemoryGate {
    pub fn new() -> Self {
        Self {
            dispatch_paused: AtomicBool::new(false),
            current_pct: AtomicU8::new(0),
        }
    }

    pub fn is_dispatch_paused(&self) -> bool {
        self.dispatch_paused.load(Ordering::Relaxed)
    }
}

/// Sample current host memory usage.
pub fn sample_memory(thresholds: &MemoryThresholds) -> MemorySnapshot {
    let mut sys = System::new();
    sys.refresh_memory();

    let total = sys.total_memory();
    let used = sys.used_memory();
    let pct = if total > 0 {
        ((used as f64 / total as f64) * 100.0) as u8
    } else {
        0
    };

    let pressure = if pct >= thresholds.hard_limit_pct {
        MemoryPressure::Emergency
    } else if pct >= thresholds.soft_limit_pct {
        MemoryPressure::Pressure
    } else if pct >= thresholds.warn_pct {
        MemoryPressure::Warning
    } else {
        MemoryPressure::Normal
    };

    MemorySnapshot {
        total_bytes: total,
        used_bytes: used,
        used_pct: pct,
        pressure,
    }
}

/// Run the memory watchdog loop.
///
/// Samples memory every `interval`, updates the `MemoryGate`, emits events,
/// and triggers emergency session stops when the hard limit is breached.
pub async fn watchdog_loop(
    gate: Arc<MemoryGate>,
    thresholds: MemoryThresholds,
    event_bus: Arc<EventBus>,
    session_manager: Arc<tasks_session::SessionManager<runtime::AppleContainerRuntime>>,
    interval: std::time::Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    let mut last_pressure = MemoryPressure::Normal;
    let mut emergency_stops: u32 = 0;

    loop {
        ticker.tick().await;

        let snapshot = sample_memory(&thresholds);
        gate.current_pct.store(snapshot.used_pct, Ordering::Relaxed);

        match snapshot.pressure {
            MemoryPressure::Normal => {
                if last_pressure >= MemoryPressure::Pressure {
                    info!(
                        used_pct = snapshot.used_pct,
                        "memory pressure resolved, resuming dispatch"
                    );
                }
                gate.dispatch_paused.store(false, Ordering::Relaxed);
            }
            MemoryPressure::Warning => {
                if last_pressure < MemoryPressure::Warning {
                    warn!(
                        used_pct = snapshot.used_pct,
                        total_gb = snapshot.total_bytes / (1024 * 1024 * 1024),
                        used_gb = snapshot.used_bytes / (1024 * 1024 * 1024),
                        "host memory usage above warning threshold"
                    );
                    emit_memory_event(&event_bus, EventType::SystemMemoryWarning, &snapshot).await;
                }
                gate.dispatch_paused.store(false, Ordering::Relaxed);
            }
            MemoryPressure::Pressure => {
                if last_pressure < MemoryPressure::Pressure {
                    warn!(
                        used_pct = snapshot.used_pct,
                        threshold = thresholds.soft_limit_pct,
                        "memory pressure: pausing new session dispatch"
                    );
                    emit_memory_event(&event_bus, EventType::SystemMemoryPressure, &snapshot).await;
                }
                gate.dispatch_paused.store(true, Ordering::Relaxed);
            }
            MemoryPressure::Emergency => {
                error!(
                    used_pct = snapshot.used_pct,
                    threshold = thresholds.hard_limit_pct,
                    "EMERGENCY: memory critical, stopping sessions to prevent OS lockup"
                );
                gate.dispatch_paused.store(true, Ordering::Relaxed);
                emit_memory_event(&event_bus, EventType::SystemMemoryEmergency, &snapshot).await;

                // Stop the most recently started session to free memory.
                // We stop one at a time — the next tick will re-evaluate.
                if let Some(task_id) = pick_session_to_stop(&session_manager).await {
                    warn!(task_id = %task_id, "emergency-stopping session due to memory pressure");
                    if let Err(e) = session_manager.stop_session(&task_id).await {
                        error!(task_id = %task_id, error = %e, "failed to emergency-stop session");
                    } else {
                        emergency_stops += 1;
                        info!(
                            task_id = %task_id,
                            total_emergency_stops = emergency_stops,
                            "session stopped to relieve memory pressure"
                        );
                    }
                }
            }
        }

        last_pressure = snapshot.pressure;
    }
}

/// Pick the best session to stop under memory pressure.
///
/// Strategy: stop the most recently started session (least work invested).
async fn pick_session_to_stop(
    session_manager: &tasks_session::SessionManager<runtime::AppleContainerRuntime>,
) -> Option<String> {
    session_manager.newest_session().await
}

/// Emit a memory-related event.
async fn emit_memory_event(event_bus: &EventBus, event_type: EventType, snapshot: &MemorySnapshot) {
    let event = Event::new(
        event_type,
        "system",
        Actor::System,
        serde_json::json!({
            "used_pct": snapshot.used_pct,
            "total_bytes": snapshot.total_bytes,
            "used_bytes": snapshot.used_bytes,
        }),
    );
    if let Err(e) = event_bus.publish(event).await {
        error!(error = %e, "failed to publish memory event");
    }
}
