//! Strict package-name validation for untrusted input.
//!
//! The C++ daemon only rejects an empty package name
//! (`GameRegistry::validate_game_entry`). That is documented as finding 7
//! in the user's analysis: a package name is concatenated to
//! `/data/data/` and written space-separated to `gameinfo`, so it must
//! not be trusted blindly.
//!
//! This module implements the stricter rule required by the task guide
//! (section 3.4): ASCII letters, digits, `_`, and `.` only. It is a pure
//! validator for Stage 2 and later; it is not yet wired into the daemon,
//! so applying it does not change Stage 1 behaviour.
//!
//! // TODO(verify): confirm with the upstream project whether a package
//! name may legitimately start or end with `.`, or contain a segment
//! that is empty. The current rule allows those cases, matching the
//! guide's stated pattern exactly, rather than inventing a stricter one.

use core::fmt;

/// Maximum package-name length, in bytes.
///
/// Android package names are well under this in practice; the limit exists
/// so a hostile config cannot make the daemon allocate or log an unbounded
/// string. It is a Stage 1 choice, not a value verified from the C++ code.
pub const MAX_PACKAGE_NAME_LEN: usize = 255;

/// An error from package-name validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PackageNameError {
    /// The package name was empty.
    #[error("package name is empty")]
    Empty,
    /// The package name was longer than [`MAX_PACKAGE_NAME_LEN`].
    #[error("package name exceeds the maximum length")]
    TooLong,
    /// The package name contained a character outside `[A-Za-z0-9_.]`.
    #[error("package name contains an invalid character")]
    InvalidCharacter,
}

/// A validated Android package name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackageName(String);

impl PackageName {
    /// Validate `value` and, on success, wrap it.
    ///
    /// # Errors
    ///
    /// Returns [`PackageNameError`] if the value is empty, too long, or
    /// contains a character outside `[A-Za-z0-9_.]`.
    pub fn new(value: &str) -> Result<Self, PackageNameError> {
        if value.is_empty() {
            return Err(PackageNameError::Empty);
        }
        if value.len() > MAX_PACKAGE_NAME_LEN {
            return Err(PackageNameError::TooLong);
        }
        let valid = value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        if !valid {
            return Err(PackageNameError::InvalidCharacter);
        }
        Ok(Self(value.to_owned()))
    }

    /// The validated package name, as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{PackageName, PackageNameError};

    #[test]
    fn accepts_typical_package_names() {
        for name in ["com.example.game", "com.example_game1", "a.b"] {
            assert!(PackageName::new(name).is_ok(), "should accept {name}");
        }
    }

    #[test]
    fn rejects_empty_path_traversal_and_spaces() {
        assert_eq!(PackageName::new(""), Err(PackageNameError::Empty));
        assert_eq!(
            PackageName::new("../etc"),
            Err(PackageNameError::InvalidCharacter)
        );
        assert_eq!(
            PackageName::new("com.example game"),
            Err(PackageNameError::InvalidCharacter)
        );
        assert_eq!(
            PackageName::new("com/example"),
            Err(PackageNameError::InvalidCharacter)
        );
    }

    #[test]
    fn rejects_overlong_names() {
        let long = "a".repeat(super::MAX_PACKAGE_NAME_LEN + 1);
        assert_eq!(PackageName::new(&long), Err(PackageNameError::TooLong));
    }
}
