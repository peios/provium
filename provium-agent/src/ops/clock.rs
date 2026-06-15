//! Clock-family op handlers — slice 11 minimum.
//!
//! Implements `GetTime`, `SetTime`, `SleepClock`, `AdvanceClock`
//! via the standard POSIX clock APIs (`clock_gettime` /
//! `clock_settime` / `nanosleep`). `AdvanceClock` is implemented
//! as get-then-set; the design's "bump by N" semantics apply only
//! to wall clock, not to monotonic time, so this is sufficient.

use std::io;

use provium_protocol::wire::{
    AdvanceClockArgs, AgentMessage, ClockTime, GetTimeArgs, OpResult, SetTimeArgs,
    SleepClockArgs,
};

use super::os_error_from_io;

/// `GetTime` — read CLOCK_REALTIME.
pub fn get_time(_args: GetTimeArgs) -> AgentMessage {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime is async-signal-safe and writes to the
    // address we pass in. CLOCK_REALTIME is always defined.
    let r = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    if r != 0 {
        return AgentMessage::GetTimeResult(OpResult::Err(os_error_from_io(
            io::Error::last_os_error(),
        )));
    }
    // tv_sec/tv_nsec types vary by target (i64 on x86_64, i32 on
    // 32-bit). The cast is a no-op on the target we ship and a
    // widening on others — clippy's per-target nag isn't useful
    // here.
    #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
    let ns = i64::from(ts.tv_sec as i64)
        .saturating_mul(1_000_000_000)
        .saturating_add(i64::from(ts.tv_nsec as i64));
    AgentMessage::GetTimeResult(OpResult::Ok(ClockTime { ns }))
}

/// `SetTime` — set CLOCK_REALTIME to `args.ns`. Requires CAP_SYS_TIME
/// in the guest; surfaces EPERM cleanly when missing.
pub fn set_time(args: SetTimeArgs) -> AgentMessage {
    let ts = libc::timespec {
        tv_sec: (args.ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (args.ns % 1_000_000_000) as libc::c_long,
    };
    // SAFETY: passing a valid timespec to clock_settime.
    let r = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
    if r != 0 {
        return AgentMessage::SetTimeResult(OpResult::Err(os_error_from_io(
            io::Error::last_os_error(),
        )));
    }
    AgentMessage::SetTimeResult(OpResult::Ok(()))
}

/// `SleepClock` — pause for `args.ns`.
pub fn sleep_clock(args: SleepClockArgs) -> AgentMessage {
    let secs = args.ns / 1_000_000_000;
    let nanos = (args.ns % 1_000_000_000) as u32;
    std::thread::sleep(std::time::Duration::new(secs, nanos));
    AgentMessage::SleepClockResult(OpResult::Ok(()))
}

/// `AdvanceClock` — relative bump. Read + add + set. Surfaces
/// EPERM if SetTime fails.
pub fn advance_clock(args: AdvanceClockArgs) -> AgentMessage {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let r = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    if r != 0 {
        return AgentMessage::AdvanceClockResult(OpResult::Err(os_error_from_io(
            io::Error::last_os_error(),
        )));
    }
    // On Linux x86_64 these are i64 already; on 32-bit they're
    // narrower. The lint disagrees per-target, so suppress at the
    // call site.
    #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
    let now_ns = i64::from(ts.tv_sec as i64)
        .saturating_mul(1_000_000_000)
        .saturating_add(i64::from(ts.tv_nsec as i64));
    let target = now_ns.saturating_add(args.ns);
    let target_ts = libc::timespec {
        tv_sec: (target / 1_000_000_000) as libc::time_t,
        tv_nsec: (target % 1_000_000_000) as libc::c_long,
    };
    let r = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &target_ts) };
    if r != 0 {
        return AgentMessage::AdvanceClockResult(OpResult::Err(os_error_from_io(
            io::Error::last_os_error(),
        )));
    }
    AgentMessage::AdvanceClockResult(OpResult::Ok(()))
}
