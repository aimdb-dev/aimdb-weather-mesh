//! What shape can the consumer half of an FFI binding actually take?
use aimdb_core::{buffer::BufferCfg, AimDbBuilder};
use aimdb_sync::AimDbBuilderSyncExt;
use aimdb_tokio_adapter::TokioRecordRegistrarExt;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
struct Reading {
    n: u32,
}

fn main() {
    let mut builder = AimDbBuilder::new().runtime(Arc::new(TokioAdapterNew()));
    builder.configure::<Reading>("readings", |reg| {
        reg.buffer(BufferCfg::SpmcRing { capacity: 32 })
            .tap(|_ctx, _c| async move {});
    });
    let handle = Arc::new(builder.attach().expect("attach"));

    // --- 1. can N threads each create their own consumer, concurrently? ---
    let threads = 4usize;
    let barrier = Arc::new(Barrier::new(threads + 1));
    let mut workers = Vec::new();
    for id in 0..threads {
        let handle = Arc::clone(&handle);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            // `consumer()` takes &self on a Sync handle, so this is legal from
            // every thread at once. Each call is a fresh subscription.
            let mut consumer = handle
                .consumer::<Reading>("readings")
                .expect("consumer from worker thread");
            barrier.wait(); // everyone subscribed before anything is produced
            let mut seen = Vec::new();
            for _ in 0..5 {
                match consumer.get_with_timeout(Duration::from_secs(5)) {
                    Ok(r) => seen.push(r.n),
                    Err(e) => {
                        println!("  thread {id}: {e}");
                        break;
                    }
                }
            }
            (id, seen)
        }));
    }

    barrier.wait();
    let producer = handle.producer::<Reading>("readings").expect("producer");
    for n in 0..5u32 {
        producer.set(Reading { n }).expect("set");
    }

    println!("1. N threads, one consumer each, created concurrently:");
    let mut all_saw_everything = true;
    for w in workers {
        let (id, seen) = w.join().expect("join");
        println!("   thread {id} saw {seen:?}");
        if seen != vec![0, 1, 2, 3, 4] {
            all_saw_everything = false;
        }
    }
    println!(
        "   => every consumer has its own cursor: {}",
        if all_saw_everything { "YES" } else { "NO" }
    );

    // --- 2. what does creating a consumer cost? ---
    let rounds = 1000;
    let started = Instant::now();
    for _ in 0..rounds {
        let _c = handle.consumer::<Reading>("readings").expect("consumer");
    }
    let each = started.elapsed() / rounds;
    println!("\n2. cost of handle.consumer(): {each:?} per call over {rounds} calls");

    // --- 3. is the handle itself shareable, which is what makes 1 possible? ---
    fn assert_sync<T: Sync>() {}
    assert_sync::<aimdb_sync::AimDbHandle>();
    println!("\n3. AimDbHandle is Sync: YES (compile-time)");

    // --- 4. do N blocking consumers cost N runtime workers? ---
    // Waiter::block_on is `Handle::block_on`, which drives the future on the
    // *calling* thread. If that were `Runtime::block_on` or a spawn, a C++ hub
    // with many subscriber threads would starve the runtime. This is the shape
    // that would expose it.
    let many = 16usize;
    let gate = Arc::new(Barrier::new(many + 1));
    let mut subs = Vec::new();
    for id in 0..many {
        let handle = Arc::clone(&handle);
        let gate = Arc::clone(&gate);
        subs.push(thread::spawn(move || {
            let mut consumer = handle.consumer::<Reading>("readings").expect("consumer");
            gate.wait();
            // Every one of these parks its own thread inside get().
            consumer
                .get_with_timeout(Duration::from_secs(5))
                .map(|r| r.n)
                .map_err(|e| format!("thread {id}: {e}"))
        }));
    }
    gate.wait();
    thread::sleep(Duration::from_millis(100)); // let all 16 reach get()
    producer.set(Reading { n: 99 }).expect("set");

    let served = subs
        .into_iter()
        .map(|s| s.join().expect("join"))
        .filter(|r| matches!(r, Ok(99)))
        .count();
    println!("\n4. {many} threads blocked in get() at once: {served}/{many} served");

    // --- 5. the workaround for a genuinely shared consumer ---
    // What a caller writes today if they want ONE stream split across workers
    // rather than one cursor each. `Mutex<T>` is `Sync` whenever `T: Send`, so
    // this compiles even though SyncConsumer is not Sync.
    let shared = Arc::new(std::sync::Mutex::new(
        handle.consumer::<Reading>("readings").expect("consumer"),
    ));
    for n in 100..103u32 {
        producer.set(Reading { n }).expect("set");
    }
    let mut splitters = Vec::new();
    for _ in 0..3 {
        let shared = Arc::clone(&shared);
        splitters.push(thread::spawn(move || {
            let mut guard = shared.lock().expect("lock");
            guard.get_with_timeout(Duration::from_secs(5)).map(|r| r.n)
        }));
    }
    let mut split: Vec<u32> = splitters
        .into_iter()
        .filter_map(|s| s.join().expect("join").ok())
        .collect();
    split.sort_unstable();
    println!("5. Arc<Mutex<SyncConsumer>> across 3 threads: each value went to exactly one worker: {split:?}");

    handle_detach(handle);
}

fn handle_detach(handle: Arc<aimdb_sync::AimDbHandle>) {
    match Arc::try_unwrap(handle) {
        Ok(h) => h.detach().expect("detach"),
        Err(_) => println!("\n(handle still shared; letting Drop shut it down)"),
    }
}

#[allow(non_snake_case)]
fn TokioAdapterNew() -> aimdb_tokio_adapter::TokioAdapter {
    aimdb_tokio_adapter::TokioAdapter
}
