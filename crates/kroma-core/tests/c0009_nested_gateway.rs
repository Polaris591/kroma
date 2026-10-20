//! C00-09: nested calls are admitted by one gateway, and the kernel bounds actually bite.
//!
//! The defect this pins: `tool/nested.rs` had limits and no consumer, so nothing in the product was
//! bounded by it. The codemode program is the nested call; it is admitted before it starts, settles
//! when it ends, and runs under CPU and address-space limits the kernel enforces.

use std::time::Duration;

use kroma_core::budget::BudgetConfig;
use kroma_core::tool::codemode::program_command;
use kroma_core::tool::nested::{NestedError, NestedGateway, NestedLimits};

fn limits() -> NestedLimits {
    NestedLimits {
        max_depth: 1,
        max_calls: 2,
        max_result_bytes: 64,
        deadline: Duration::from_secs(5),
        max_concurrent: 1,
        max_cpu_seconds: 30,
        max_memory_bytes: 1024 * 1024 * 1024,
    }
}

/// C00-09-A1: depth, call allowance, concurrency and result size are each refused, by name.
#[test]
fn the_gateway_refuses_each_limit_by_name() {
    let gateway = NestedGateway::new(limits());
    assert!(matches!(
        gateway.begin(2),
        Err(NestedError::DepthExceeded { max_depth: 1, .. })
    ));

    let first = gateway.begin(0).expect("the first call is admitted");
    assert!(matches!(
        gateway.begin(0),
        Err(NestedError::ConcurrencyExceeded { max_concurrent: 1 })
    ));
    gateway.settle(first, 10).expect("a small result settles");

    let second = gateway.begin(0).expect("the second call is admitted");
    gateway.settle(second, 10).expect("settles");
    assert!(matches!(
        gateway.begin(0),
        Err(NestedError::CallBudgetExhausted { max_calls: 2 })
    ));

    let third = NestedGateway::new(limits());
    let ticket = third.begin(0).expect("admitted");
    assert!(matches!(
        third.settle(ticket, 65),
        Err(NestedError::ResultTooLarge {
            max_result_bytes: 64,
            ..
        })
    ));
}

/// C00-09-A2: the CPU and address-space limits are the kernel's, not a wish. A program that spins or
/// allocates past them is killed instead of being allowed to run to completion.
#[test]
fn the_kernel_bounds_stop_a_runaway_program() {
    let root = std::env::temp_dir().join(format!("kroma-c0009-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("scratch");

    // Address space: ask for 512 MiB with a 64 MiB ceiling.
    let command = program_command(
        &root.to_string_lossy(),
        "block = bytearray(512 * 1024 * 1024)\nprint('allocated')",
        30,
        64 * 1024 * 1024,
    );
    let outcome = kroma_core::sandbox::run_bounded(
        "sh",
        &["-c".to_string(), command],
        Duration::from_secs(30),
    )
    .expect("the shell itself completes");
    assert!(
        !outcome.success,
        "allocating past the address-space limit must kill the program, got: {}",
        outcome.stdout
    );

    // CPU: a one-second budget against a spin loop that would otherwise never end.
    let command = program_command(
        &root.to_string_lossy(),
        "while True:\n    pass",
        1,
        256 * 1024 * 1024,
    );
    let started = std::time::Instant::now();
    let outcome = kroma_core::sandbox::run_bounded(
        "sh",
        &["-c".to_string(), command],
        Duration::from_secs(20),
    )
    .expect("the shell completes");
    assert!(
        !outcome.success,
        "a spin loop must be killed by the CPU limit"
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the CPU limit must end the program well before the wall clock would"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// C00-09-A3: a program interrupted mid-flight leaves its non-idempotent side effect exactly once.
#[test]
fn an_interrupted_program_does_not_repeat_its_side_effect() {
    let root = std::env::temp_dir().join(format!("kroma-c0009-once-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("scratch");
    let marker = root.join("marker.txt");
    let command = program_command(
        &root.to_string_lossy(),
        "write('marker.txt', 'one\\n')\nimport time\ntime.sleep(30)\nwrite('marker.txt', read('marker.txt') + 'two\\n')",
        30,
        256 * 1024 * 1024,
    );
    // The supervisor kills the tree at the deadline; the second write must never happen.
    let _ = kroma_core::sandbox::run_bounded(
        "sh",
        &["-c".to_string(), command],
        Duration::from_millis(700),
    );

    let contents = std::fs::read_to_string(&marker).expect("the first write happened");
    assert_eq!(
        contents, "one\n",
        "an interrupted program must not repeat or complete its side effect"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// C00-09-A4: the per-run limits come from the run budget, not from a constant nobody reads.
#[test]
fn the_run_limits_are_derived_from_the_budget() {
    let budget = BudgetConfig {
        max_tool_calls: 7,
        ..BudgetConfig::default()
    };
    let limits = NestedLimits::for_run(&budget);
    assert_eq!(limits.max_calls, 7);
    assert_eq!(limits.max_depth, 1, "a nested call may not itself nest");
    assert_eq!(limits.max_concurrent, 1);
    assert!(limits.max_cpu_seconds > 0 && limits.max_memory_bytes > 0);
}
