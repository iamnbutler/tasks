//! Integration tests for the pool using real supervisor processes.
//!
//! These tests spawn actual supervisor binaries, send real commands,
//! and verify events propagate through the event log. No mocks.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;
use vm_pool_manager::{
    Event, EventLog, EventPayload, ImageRef, Pool, PoolConfig, SupervisorRuntime, VmState,
};
use vm_pool_protocol::{Priority, ShellCommand, ShellEvent, ShellProtocol, VmConfig, VmId};
use vm_pool_test_support::supervisor_binary;

/// How long [`await_vm_events`] waits before reporting what it has.
///
/// A stall detector, not a schedule — nothing here is expected to spend any of
/// it. See [`await_vm_events`].
const AWAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Wait until this VM's events satisfy `done`, then return them.
///
/// These tests used to `sleep` a fixed span and assert over whatever had
/// arrived. What they were waiting for is a chain of process forks — the
/// supervisor awaits each `Execute` before reading the next line, and each one
/// forks `sh` — so the sleep was a claim about how fast a loaded machine
/// forks. Under `cargo nextest` against a freshly linked binary those forks
/// were observed at 75–162ms apiece, which is how the four commands below
/// overran a 500ms sleep and how these tests came to fail in parallel and pass
/// alone.
///
/// Subscribing *before* the first snapshot is what makes this a wait and not a
/// poll: no append can fall between the two, so the loop sleeps until an event
/// arrives and wakes on the one that satisfies it.
///
/// The ceiling stays because a wait that ended *only* on success would hang a
/// genuine regression instead of reporting it. On expiry this returns what the
/// log holds and leaves the caller's own assertion to say what was missing —
/// the same message a fixed sleep gave, minus the false alarms.
///
/// Duplicated from the pool crate's own test module rather than shared through
/// `vm-pool-test-support`, on the same grounds as `RecordingRuntime`: that
/// crate is a dev-dependency of this one, and a shared home would need a
/// dev-dependency cycle to reach `EventLog`.
async fn await_vm_events<F>(
    events: &EventLog<ShellProtocol>,
    vm_id: &VmId,
    mut done: F,
) -> Vec<Event<ShellProtocol>>
where
    F: FnMut(&[Event<ShellProtocol>]) -> bool,
{
    let mut rx = events.subscribe();
    let deadline = tokio::time::Instant::now() + AWAIT_TIMEOUT;
    loop {
        let snapshot = events.for_vm(vm_id).await;
        if done(&snapshot) {
            return snapshot;
        }
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(_)) => {}
            // Lagged only means this loop missed a wakeup; the log is the
            // source of truth and is re-read at the top regardless.
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            // Nothing more is coming, or the ceiling was reached.
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                return events.for_vm(vm_id).await;
            }
        }
    }
}

/// The application events among a VM's log entries, oldest first.
fn app_events(events: &[Event<ShellProtocol>]) -> Vec<ShellEvent> {
    events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::VmApp { event, .. } => Some(event.clone()),
            _ => None,
        })
        .collect()
}

/// The lifecycle states among a VM's log entries, oldest first.
fn lifecycle_states(events: &[Event<ShellProtocol>]) -> Vec<VmState> {
    events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::VmLifecycle { state, .. } => Some(*state),
            _ => None,
        })
        .collect()
}

/// How many commands have reported completion.
fn completions(events: &[Event<ShellProtocol>]) -> Vec<i32> {
    app_events(events)
        .iter()
        .filter_map(|e| match e {
            ShellEvent::CommandCompleted { exit_code } => Some(*exit_code),
            _ => None,
        })
        .collect()
}

fn make_pool(
    binary: &PathBuf,
    max_vms: usize,
    events: Arc<EventLog<ShellProtocol>>,
) -> Arc<Pool<SupervisorRuntime, ShellProtocol>> {
    Pool::with_runtime(
        PoolConfig {
            max_vms,
            health_check_interval: 300,
            vm_timeout: 7200,
        },
        events,
        SupervisorRuntime::new(binary),
    )
}

fn config_with_priority(priority: Priority) -> VmConfig {
    VmConfig {
        priority,
        ..Default::default()
    }
}

/// Test: Allocate a real VM, execute a command, verify output in the event log.
#[tokio::test]
async fn allocate_execute_and_verify_events() {
    let binary = supervisor_binary();
    let events = EventLog::<ShellProtocol>::new();
    let pool = make_pool(&binary, 3, events.clone());

    // Allocate a VM
    let vm_id = pool
        .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
        .await
        .unwrap();

    // Execute a command
    pool.send_to_vm(
        &vm_id,
        ShellCommand::Execute {
            command: "echo integration-test-output".into(),
        },
    )
    .await
    .unwrap();

    // Wait for the command to report completion, rather than for a span of
    // time in which it is hoped to.
    let vm_events = await_vm_events(&events, &vm_id, |evts| !completions(evts).is_empty()).await;

    // Should have lifecycle events (Allocating, Ready) + application events (Output, CommandCompleted)
    let lifecycle_events = lifecycle_states(&vm_events);
    assert!(
        lifecycle_events.contains(&VmState::Allocating),
        "missing Allocating event"
    );
    assert!(
        lifecycle_events.contains(&VmState::Ready),
        "missing Ready event"
    );

    let app_events = app_events(&vm_events);
    assert!(
        !app_events.is_empty(),
        "expected app events from Execute command"
    );

    // Verify the actual output contains our marker
    let has_output = app_events.iter().any(|e| match e {
        ShellEvent::Output { data, .. } => data.contains("integration-test-output"),
        _ => false,
    });
    assert!(
        has_output,
        "expected output containing 'integration-test-output', got: {:?}",
        app_events
    );

    // Verify CommandCompleted with exit code 0
    let has_completed = app_events
        .iter()
        .any(|e| matches!(e, ShellEvent::CommandCompleted { exit_code: 0 }));
    assert!(
        has_completed,
        "expected CommandCompleted with exit_code 0, got: {:?}",
        app_events
    );

    // Deallocate
    pool.deallocate(&vm_id).await.unwrap();
    assert_eq!(pool.status().await.allocated, 0);
}

/// Test: Fill the pool, then evict a low-priority VM for a high-priority one.
#[tokio::test]
async fn priority_eviction() {
    let binary = supervisor_binary();
    let events = EventLog::<ShellProtocol>::new();
    let pool = make_pool(&binary, 2, events.clone());

    // Fill the pool with low-priority VMs
    let low1 = pool
        .allocate(
            ImageRef::new("agent", "v1"),
            config_with_priority(Priority::Low),
        )
        .await
        .unwrap();

    let low2 = pool
        .allocate(
            ImageRef::new("agent", "v1"),
            config_with_priority(Priority::Low),
        )
        .await
        .unwrap();

    assert_eq!(pool.status().await.allocated, 2);
    assert_eq!(pool.status().await.available, 0);

    // Try to allocate a high-priority VM — should evict one of the low ones
    let (high_id, evicted) = pool
        .allocate_or_evict(
            ImageRef::new("agent", "v1"),
            config_with_priority(Priority::High),
        )
        .await
        .unwrap();

    assert!(evicted.is_some(), "expected a VM to be evicted");
    let evicted_id = evicted.unwrap();
    assert!(
        evicted_id == low1 || evicted_id == low2,
        "evicted VM should be one of the low-priority ones"
    );

    // Pool should still be at capacity with the high-priority VM
    assert_eq!(pool.status().await.allocated, 2);

    // The evicted VM should be gone
    assert_eq!(pool.get(&evicted_id).await, None);
    // The high-priority VM should be ready
    assert_eq!(pool.get(&high_id).await, Some(VmState::Ready));

    // Verify the evicted VM has Stopping/Stopped events in the log. No wait:
    // the eviction is a `deallocate` this call already awaited, and both
    // events are appended before it returns.
    let evicted_states = lifecycle_states(&events.for_vm(&evicted_id).await);
    assert!(
        evicted_states.contains(&VmState::Stopping),
        "evicted VM should have Stopping event"
    );
    assert!(
        evicted_states.contains(&VmState::Stopped),
        "evicted VM should have Stopped event"
    );

    // Clean up
    pool.deallocate(&high_id).await.unwrap();
    // The remaining low-priority VM
    let remaining = if evicted_id == low1 { low2 } else { low1 };
    pool.deallocate(&remaining).await.unwrap();
    assert_eq!(pool.status().await.allocated, 0);
}

/// Test: Cannot evict when all VMs have equal or higher priority.
#[tokio::test]
async fn no_eviction_when_all_same_priority() {
    let binary = supervisor_binary();
    let events = EventLog::<ShellProtocol>::new();
    let pool = make_pool(&binary, 1, events);

    pool.allocate(
        ImageRef::new("agent", "v1"),
        config_with_priority(Priority::Normal),
    )
    .await
    .unwrap();

    // Try to evict with same priority — should fail
    let result = pool
        .allocate_or_evict(
            ImageRef::new("agent", "v1"),
            config_with_priority(Priority::Normal),
        )
        .await;
    assert!(
        matches!(result, Err(vm_pool_manager::PoolError::Exhausted { .. })),
        "should not evict VMs of equal priority"
    );
}

/// Test: Full lifecycle — allocate, execute multiple commands, deallocate,
/// verify all events in correct order.
#[tokio::test]
async fn full_lifecycle_with_multiple_commands() {
    let binary = supervisor_binary();
    let events = EventLog::<ShellProtocol>::new();
    let pool = make_pool(&binary, 3, events.clone());

    let vm_id = pool
        .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
        .await
        .unwrap();

    // Execute several commands
    for i in 0..3 {
        pool.send_to_vm(
            &vm_id,
            ShellCommand::Execute {
                command: format!("echo cmd-{}", i),
            },
        )
        .await
        .unwrap();
    }

    // Execute a failing command
    pool.send_to_vm(
        &vm_id,
        ShellCommand::Execute {
            command: "exit 7".into(),
        },
    )
    .await
    .unwrap();

    // The supervisor runs these serially, one `sh` fork each, so what bounds
    // this is four process spawns and not any interval worth naming.
    let vm_events = await_vm_events(&events, &vm_id, |evts| completions(evts).len() >= 4).await;
    let completed = completions(&vm_events);
    assert_eq!(
        completed.len(),
        4,
        "expected 4 CommandCompleted events, got {:?}",
        completed
    );
    // First 3 should succeed, last should be exit code 7
    assert_eq!(completed[0], 0);
    assert_eq!(completed[1], 0);
    assert_eq!(completed[2], 0);
    assert_eq!(completed[3], 7);

    // Deallocate
    pool.deallocate(&vm_id).await.unwrap();

    // No wait here, and none needed: `deallocate` appends both Stopping and
    // Stopped before it returns, and the VM it removed from the map is one
    // slot reclamation will not append `Crashed` for. Sleeping would only
    // widen the window for something else to land in a sequence asserted
    // whole.
    let states = lifecycle_states(&events.for_vm(&vm_id).await);
    assert_eq!(
        states,
        vec![
            VmState::Allocating,
            VmState::Ready,
            VmState::Stopping,
            VmState::Stopped,
        ]
    );
}

/// Test: Multiple VMs running concurrently, each executing commands.
#[tokio::test]
async fn concurrent_vms() {
    let binary = supervisor_binary();
    let events = EventLog::<ShellProtocol>::new();
    let pool = make_pool(&binary, 3, events.clone());

    // Allocate 3 VMs
    let mut vm_ids = Vec::new();
    for _ in 0..3 {
        let id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        vm_ids.push(id);
    }

    assert_eq!(pool.status().await.allocated, 3);

    // Send a unique command to each VM
    for (i, vm_id) in vm_ids.iter().enumerate() {
        pool.send_to_vm(
            vm_id,
            ShellCommand::Execute {
                command: format!("echo vm-{}-output", i),
            },
        )
        .await
        .unwrap();
    }

    // Each VM should have its own events. Waited for one at a time, which
    // costs nothing: the three run concurrently, so whichever is slowest is
    // what this ends on however the loop is ordered.
    for (i, vm_id) in vm_ids.iter().enumerate() {
        let marker = format!("vm-{}-output", i);
        let carries_its_marker = |evts: &[Event<ShellProtocol>]| {
            app_events(evts)
                .iter()
                .any(|e| matches!(e, ShellEvent::Output { data, .. } if data.contains(&marker)))
        };
        let vm_events = await_vm_events(&events, vm_id, carries_its_marker).await;
        assert!(
            carries_its_marker(&vm_events),
            "VM {} missing its unique output in events",
            vm_id
        );
    }

    // Deallocate all
    for vm_id in &vm_ids {
        pool.deallocate(vm_id).await.unwrap();
    }
    assert_eq!(pool.status().await.allocated, 0);
}
