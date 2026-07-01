//! Per-command spawn-cost benchmark for the shell backends.
//!
//! Measures the **per-command cost the agent pays** for the three execution
//! models, on an identical workload of simple read-only commands:
//!
//! - `raw_bash_c` — a bare `bash -c <cmd>` (null stdin, piped capture). The floor:
//!   process-spawn cost with zero just-agent machinery. (The `stateless_exec`
//!   gap over this floor is not "just logic" — stateless additionally pays for
//!   `process_group(0)`, file-input spawn `bash <wrapper>` vs `bash -c`, and the
//!   wrapper's per-call `EXIT` trap.)
//! - `stateless_exec` — the new stateless backend's `exec`: a fresh `bash` per
//!   call (wrapper file + color-env injection + run + cwd trap + pgroup reap).
//! - `pty_exec` — the original persistent-PTY backend's `execute`: no per-call
//!   spawn, but a stability-poll (default 3 × 100 ms) gates every return.
//!
//! ## Methodology
//!
//! Only the **inner command execution** is timed. For the two session-based
//! backends each iteration creates a fresh session (build, untimed), runs the
//! command (timed), and tears the session down (untimed) — so thousands of
//! criterion iterations never accumulate living sessions ("session explosion").
//!
//! This relies on criterion's `iter_batched_ref`: per the criterion 0.8 source,
//! the entire batch is filled by `setup` *before* the timed region starts and
//! dropped *after* it ends — so neither session build nor `Drop` is charged to a
//! sample, under any `BatchSize`. The current-thread tokio runtime is built
//! *inside* each `bench_function` closure (on criterion's bench thread), because
//! `Runtime::block_on` only drives its drivers on the owning thread.
//!
//! Run with: `cargo bench -p just-agent-shell`.

use std::hint::black_box;
use std::process::Stdio;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use just_agent_shell::{PtyBuilder, ShellBackend, StatelessBackend, StatelessBuilder};

/// Generous timeout — none of the read-only pool commands approach it.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Fixed xorshift seed shared across all three benches so each runs the
/// *identical* command sequence (apples-to-apples). Non-zero (xorshift requires
/// it). Each bench holds its own `state` initialized from this constant.
const SEED: u64 = 0x9e37_79b9_7f4a_7c15;

/// Simple, read-only, fast commands. Picked per iteration so the measured cost
/// reflects a realistic mix rather than a single command.
const POOL: &[&str] = &[
    "ls",
    "pwd",
    "whoami",
    "echo hi",
    "true",
    "id",
    "uname",
    "cat /dev/null",
    "printf x",
    "test -d /",
];

/// Advance a 64-bit xorshift PRNG and map the result onto the command pool.
fn pick(state: &mut u64) -> usize {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    (x as usize) % POOL.len()
}

/// The first `n` commands the shared workload picks — printed once so the three
/// benches can be eyeballed to agree on sequence.
fn first_picks(n: usize) -> Vec<&'static str> {
    let mut state = SEED;
    (0..n).map(|_| POOL[pick(&mut state)]).collect()
}

/// A current-thread tokio runtime, built on the calling (criterion bench) thread.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

fn spawn_benches(c: &mut Criterion) {
    eprintln!(
        "[spawn-bench] shared workload, first 5 picks: {:?}",
        first_picks(5)
    );

    // One group. `sample_size(20)` (criterion minimum is 10) bounds wall-clock for
    // the ~300 ms/iter PTY bench; ample for the fast benches too.
    let mut group = c.benchmark_group("spawn");
    group.sample_size(20);

    // Bare floor: spawn + run + capture + reap. Plain `iter` — the spawn itself
    // is the measurement, nothing to exclude.
    group.bench_function("raw_bash_c", |b| {
        let rt = runtime();
        let mut state = SEED;
        b.iter(|| {
            let cmd = POOL[pick(&mut state)];
            let output = rt
                .block_on(async {
                    tokio::process::Command::new("bash")
                        .arg("-c")
                        .arg(cmd)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .output()
                        .await
                })
                .expect("bash -c spawn");
            black_box(output);
        });
    });

    // Stateless backend: rebuild the backend each iteration (untimed), time only
    // `exec` (timed), drop (untimed). `SmallInput` — each `ProcessBackend` holds
    // no resident child between calls, so there is no contention to avoid. A
    // shared data_dir is safe: execs run sequentially and each removes its own
    // per-call dir before returning.
    let data_dir = std::env::temp_dir().join(format!("ja-bench-stateless-{}", std::process::id()));
    group.bench_function("stateless_exec", |b| {
        let rt = runtime();
        let mut state = SEED;
        b.iter_batched_ref(
            || {
                rt.block_on(StatelessBuilder::new().data_dir(data_dir.clone()).build())
                    .expect("stateless backend build")
            },
            |backend| {
                let cmd = POOL[pick(&mut state)];
                let output = rt
                    .block_on(backend.exec(cmd, TIMEOUT))
                    .expect("stateless exec");
                black_box(output);
            },
            BatchSize::SmallInput,
        );
    });
    let _ = std::fs::remove_dir_all(&data_dir);

    // PTY backend (resident-session model): in real usage ONE long-lived session
    // is built once and reused across many commands — so we build a single session
    // (untimed, before the loop) and time `execute` reusing it. Plain `b.iter`,
    // NOT `iter_batched_ref`: the session must persist across iterations, and a
    // larger `BatchSize` builds *more separate sessions* (each used once), it does
    // NOT reuse one. Execution is sequential either way.
    group.bench_function("pty_exec", |b| {
        let rt = runtime();
        let mut state = SEED;
        let mut session = rt
            .block_on(PtyBuilder::new("bench").build())
            .expect("pty backend build");
        b.iter(|| {
            let cmd = POOL[pick(&mut state)];
            let output = rt
                .block_on(session.execute(cmd, TIMEOUT, false))
                .expect("pty execute");
            black_box(output);
        });
        drop(session);
    });

    group.finish();
}

criterion_group!(benches, spawn_benches);
criterion_main!(benches);
