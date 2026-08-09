//! Integration tests for the pool using real supervisor processes.
//!
//! These tests spawn actual supervisor binaries, send real commands,
//! and verify events propagate through the event log. No mocks.

use std::path::PathBuf;
use std::sync::Arc;

use vm_pool_manager::{
    EventLog, EventPayload, ImageRef, Pool, PoolConfig, SupervisorRuntime, VmState,
};
use vm_pool_protocol::{Priority, ShellCommand, ShellEvent, ShellProtocol, VmConfig};

/// Build the supervisor and return its path.
async fn build_supervisor() -> PathBuf {
    let output = tokio::process::Command::new("cargo")
        .args(["build", "-p", "vm-pool-supervisor", "--message-format=json"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "supervisor build failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|msg| {
            if msg.get("reason")?.as_str()? == "compiler-artifact"
                && msg.get("target")?.get("name")?.as_str()? == "supervisor"
            {
                Some(PathBuf::from(msg.get("executable")?.as_str()?))
            } else {
                None
            }
        })
        .expect("could not find supervisor binary path")
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
    let binary = build_supervisor().await;
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

    // Wait for events to propagate through the system
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Verify events in the log
    let vm_events = events.for_vm(&vm_id).await;

    // Should have lifecycle events (Allocating, Ready) + application events (Output, CommandCompleted)
    let lifecycle_events: Vec<_> = vm_events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::VmLifecycle { state, .. } => Some(*state),
            _ => None,
        })
        .collect();
    assert!(
        lifecycle_events.contains(&VmState::Allocating),
        "missing Allocating event"
    );
    assert!(
        lifecycle_events.contains(&VmState::Ready),
        "missing Ready event"
    );

    let app_events: Vec<_> = vm_events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::VmApp { event, .. } => Some(event.clone()),
            _ => None,
        })
        .collect();
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
    let binary = build_supervisor().await;
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

    // Verify the evicted VM has Stopping/Stopped events in the log
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let evicted_events = events.for_vm(&evicted_id).await;
    let evicted_states: Vec<_> = evicted_events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::VmLifecycle { state, .. } => Some(*state),
            _ => None,
        })
        .collect();
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
    let binary = build_supervisor().await;
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
    let binary = build_supervisor().await;
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

    // Wait for all events
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let vm_events = events.for_vm(&vm_id).await;
    let app_events: Vec<_> = vm_events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::VmApp { event, .. } => Some(event.clone()),
            _ => None,
        })
        .collect();

    // Count CommandCompleted events
    let completed: Vec<_> = app_events
        .iter()
        .filter_map(|e| match e {
            ShellEvent::CommandCompleted { exit_code } => Some(*exit_code),
            _ => None,
        })
        .collect();
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

    // Verify final lifecycle
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let final_events = events.for_vm(&vm_id).await;
    let states: Vec<_> = final_events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::VmLifecycle { state, .. } => Some(*state),
            _ => None,
        })
        .collect();
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
    let binary = build_supervisor().await;
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

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Each VM should have its own events
    for (i, vm_id) in vm_ids.iter().enumerate() {
        let vm_events = events.for_vm(vm_id).await;
        let has_output = vm_events.iter().any(|e| match &e.payload {
            EventPayload::VmApp {
                event: ShellEvent::Output { data, .. },
                ..
            } => data.contains(&format!("vm-{}-output", i)),
            _ => false,
        });
        assert!(
            has_output,
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
