//! Profile modes.
//!
//! Verified from `jni/include/Encore.hpp`:
//!
//! ```cpp
//! enum EncoreProfileMode : char {
//!     PERFCOMMON,
//!     PERFORMANCE_PROFILE,
//!     BALANCE_PROFILE,
//!     POWERSAVE_PROFILE
//! };
//! ```
//!
//! The C++ daemon writes the integer value of the mode to
//! `PROFILE_MODE` (`/data/adb/.config/encore/current_profile`) via
//! `write2file`, so the numeric codes below are part of the on-disk
//! behaviour and must not be reordered.

/// A daemon profile mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileMode {
    /// Common / idle profile, written as `0`.
    PerfCommon,
    /// Performance profile, written as `1`.
    Performance,
    /// Balance profile, written as `2`.
    Balance,
    /// Powersave profile, written as `3`.
    Powersave,
}

impl ProfileMode {
    /// The integer code written to `current_profile`.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::PerfCommon => 0,
            Self::Performance => 1,
            Self::Balance => 2,
            Self::Powersave => 3,
        }
    }

    /// Parse an integer code read back from `current_profile`.
    ///
    /// Unknown codes are rejected instead of being mapped to a default,
    /// because silently choosing a profile would be a behaviour change.
    #[must_use]
    pub const fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(Self::PerfCommon),
            1 => Some(Self::Performance),
            2 => Some(Self::Balance),
            3 => Some(Self::Powersave),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProfileMode;

    #[test]
    fn codes_match_cpp_enum_order() {
        assert_eq!(ProfileMode::PerfCommon.code(), 0);
        assert_eq!(ProfileMode::Performance.code(), 1);
        assert_eq!(ProfileMode::Balance.code(), 2);
        assert_eq!(ProfileMode::Powersave.code(), 3);
    }

    #[test]
    fn from_code_round_trips_and_rejects_unknown() {
        for mode in [
            ProfileMode::PerfCommon,
            ProfileMode::Performance,
            ProfileMode::Balance,
            ProfileMode::Powersave,
        ] {
            assert_eq!(ProfileMode::from_code(mode.code()), Some(mode));
        }
        assert_eq!(ProfileMode::from_code(-1), None);
        assert_eq!(ProfileMode::from_code(4), None);
    }
}
