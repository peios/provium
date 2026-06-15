//! Per-VM clock control — get/set wall-clock time, sleep,
//! advance (relative bump). Slice 11.

use serde::{Deserialize, Serialize};

use super::OpResult;

/// `GetTime` — read the agent's wall clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetTimeArgs;

/// Time in nanoseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockTime {
    /// Nanoseconds since 1970-01-01.
    pub ns: i64,
}

/// `GetTime` payload type.
pub type GetTimeResult = OpResult<ClockTime>;

/// `SetTime` — set the agent's wall clock to `ns` nanoseconds
/// since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetTimeArgs {
    /// Target time, ns since epoch.
    pub ns: i64,
}

/// `SetTime` payload type.
pub type SetTimeResult = OpResult<()>;

/// `SleepClock` — pause the agent for `ns` nanoseconds. Distinct
/// from a `vm:run("sleep N")` because it doesn't depend on a guest
/// shell and is implementable in pure-syscall mode (slice 11
/// minimum).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SleepClockArgs {
    /// Sleep duration, nanoseconds.
    pub ns: u64,
}

/// `SleepClock` payload type.
pub type SleepClockResult = OpResult<()>;

/// `AdvanceClock` — relative bump to the agent's wall clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvanceClockArgs {
    /// Nanoseconds to add. Negative values rewind.
    pub ns: i64,
}

/// `AdvanceClock` payload type.
pub type AdvanceClockResult = OpResult<()>;

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(v: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let bytes = rmp_serde::to_vec_named(v).unwrap();
        rmp_serde::from_slice(&bytes).unwrap()
    }

    #[test]
    fn time_round_trips() {
        let t = ClockTime { ns: 1_700_000_000_000_000_000 };
        assert_eq!(t, round_trip(&t));
    }

    #[test]
    fn each_args_struct_round_trips() {
        let a = SetTimeArgs { ns: 0 };
        assert_eq!(a, round_trip(&a));
        let b = SleepClockArgs { ns: 100 };
        assert_eq!(b, round_trip(&b));
        let c = AdvanceClockArgs { ns: -500 };
        assert_eq!(c, round_trip(&c));
    }
}
