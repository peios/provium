// Minimal sanity spike: 4 mlua states in 4 OS threads, each running a
// non-trivial Lua program in parallel. Confirms the design's threading
// model (one Lua state per file-runner thread).
//
// Pass criteria:
//   - All threads complete without panic
//   - Each returns a distinct computed value (proves they ran in parallel,
//     each in its own state, no state crosstalk)
//   - One thread's panic in Lua is caught by catch_unwind in that thread
//     without poisoning others (panic isolation per design)

use mlua::{Lua, Function};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread;
use std::time::Instant;

fn run_in_lua(thread_id: usize) -> mlua::Result<i64> {
    let lua = Lua::new();

    // Each thread defines its own globals — confirms isolation.
    lua.globals().set("THREAD_ID", thread_id as i64)?;

    let script = r#"
        -- Simulate a non-trivial test
        local sum = 0
        for i = 1, 100000 do
            sum = sum + i
        end
        local checksum = sum + (THREAD_ID * 1000000)
        return checksum
    "#;

    let f: Function = lua.load(script).into_function()?;
    let result: i64 = f.call(())?;
    Ok(result)
}

fn run_panicking_lua() -> mlua::Result<i64> {
    let lua = Lua::new();
    let script = r#"
        error("intentional panic from Lua")
    "#;
    let f: Function = lua.load(script).into_function()?;
    let result: i64 = f.call(())?;
    Ok(result)
}

fn main() {
    let started = Instant::now();
    println!("== Test 1: 4 parallel mlua states");

    let handles: Vec<_> = (0..4).map(|i| {
        thread::spawn(move || {
            (i, run_in_lua(i))
        })
    }).collect();

    let mut results = vec![];
    for h in handles {
        let (id, r) = h.join().expect("thread panicked");
        let v = r.expect("Lua execution failed");
        results.push((id, v));
    }

    let mut ok = true;
    for (id, v) in &results {
        // 100000 * 100001 / 2 = 5,000,050,000
        let expected = 5_000_050_000i64 + (*id as i64 * 1_000_000);
        let pass = *v == expected;
        if !pass { ok = false; }
        println!("  thread {} -> {} {}", id, v, if pass { "OK" } else { "WRONG" });
    }

    if !ok {
        eprintln!("FAIL: thread results did not match");
        std::process::exit(1);
    }
    println!("  parallel run took: {:.3}s", started.elapsed().as_secs_f64());

    println!("\n== Test 2: a thread's Lua error is caught, others unaffected");

    let bad_handle = thread::spawn(|| {
        catch_unwind(AssertUnwindSafe(|| run_panicking_lua()))
    });
    let good_handles: Vec<_> = (10..14).map(|i| {
        thread::spawn(move || run_in_lua(i))
    }).collect();

    let bad = bad_handle.join().expect("bad thread itself panicked");
    match bad {
        Ok(Err(e)) => println!("  bad thread: caught Lua error gracefully: {}", e),
        Ok(Ok(_)) => { eprintln!("FAIL: bad thread did not produce an error"); std::process::exit(1); }
        Err(_) => println!("  bad thread: caught panic via catch_unwind"),
    }

    let mut all_ok = true;
    for (i, h) in good_handles.into_iter().enumerate() {
        match h.join() {
            Ok(Ok(v)) => {
                let expected = 5_000_050_000i64 + ((10 + i as i64) * 1_000_000);
                let pass = v == expected;
                if !pass { all_ok = false; }
                println!("  sibling thread {} -> {} {}", 10 + i, v, if pass { "OK" } else { "WRONG" });
            },
            Ok(Err(e)) => { eprintln!("FAIL: sibling thread Lua err: {}", e); all_ok = false; }
            Err(_) => { eprintln!("FAIL: sibling thread panicked"); all_ok = false; }
        }
    }
    if !all_ok {
        std::process::exit(1);
    }

    println!("\nPASS: mlua + threads model is sound");
}
