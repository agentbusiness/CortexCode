//! Release versions (`docs/RELEASES.md`): three numbers, each 0 to 99 (`0.3.7`). The next after
//! `0.3.99` is `0.4.0`, after `0.99.99` it is `1.0.0`. Build metadata after `+`
//! (`0.3.7+b1a2b3c4d5e6`) is not compared.

use std::fmt;

/// The highest value of each part of a version.
pub const PART_MAX: u32 = 99;

/// `major.minor.patch`, each 0 to [`PART_MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// Read `v0.3.7`, `0.3.7` or `0.3.7+b1a2b`; the build metadata is ignored.
    pub fn parse(text: &str) -> Option<Version> {
        let base = text.trim().trim_start_matches('v').split('+').next()?;
        let mut parts = base.split('.').map(|part| part.parse::<u32>().ok().filter(|value| *value <= PART_MAX));
        let version = Version(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(version)
    }

    /// The next version: the last part up by one, carrying over at 99.
    pub fn next(self) -> Version {
        let Version(mut major, mut minor, mut patch) = self;
        patch += 1;
        if patch > PART_MAX {
            patch = 0;
            minor += 1;
        }
        if minor > PART_MAX {
            minor = 0;
            major += 1;
        }
        Version(major, minor, patch)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Whether `candidate` is a newer version than `current` (unreadable versions are never newer).
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        (Some(_), None) => true,
        _ => false,
    }
}
