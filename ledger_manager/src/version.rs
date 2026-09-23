//! Minimal semantic versioning helpers.
//!
//! Ledger Live relies on the `semver` npm package (`coerce`, `valid`, `satisfies`, `gt`...) to
//! compare firmware and application versions. This implements the small subset of it we need.

use std::cmp::Ordering;

/// A parsed semantic version. Build metadata is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Pre-release identifiers (the part after the `-`), if any.
    pub pre: Vec<String>,
}

impl SemVer {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: Vec::new(),
        }
    }

    /// Strictly parse a semver string ("1.2.3", "1.2.3-rc1", "1.2.3+build"), like `semver.valid`.
    /// A leading "v" or "=" is tolerated, as in the npm package.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s
            .strip_prefix('v')
            .or_else(|| s.strip_prefix('='))
            .unwrap_or(s);
        // Drop the build metadata.
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let major = parse_numeric(parts.next()?)?;
        let minor = parse_numeric(parts.next()?)?;
        let patch = parse_numeric(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            Some(pre) => {
                let ids: Vec<String> = pre.split('.').map(|s| s.to_string()).collect();
                if ids.iter().any(|id| {
                    id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                }) {
                    return None;
                }
                ids
            }
            None => Vec::new(),
        };
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// Loosely extract a version from any string, like `semver.coerce`: the first sequence of up
    /// to three dot-separated numbers is used, missing parts default to 0 and anything else
    /// (including a pre-release) is dropped. "2.1.0-rc1" gives 2.1.0, "1.16" gives 1.16.0.
    pub fn coerce(s: &str) -> Option<Self> {
        let bytes = s.as_bytes();
        let start = bytes.iter().position(|b| b.is_ascii_digit())?;
        let mut nums = Vec::with_capacity(3);
        let mut i = start;
        while nums.len() < 3 {
            let num_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if num_start == i {
                break;
            }
            // Like semver, cap each part to 16 digits.
            let num = s[num_start..i].get(..16.min(i - num_start))?.parse().ok()?;
            nums.push(num);
            if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
                i += 1;
            } else {
                break;
            }
        }
        Some(Self::new(
            nums.first().copied().unwrap_or(0),
            nums.get(1).copied().unwrap_or(0),
            nums.get(2).copied().unwrap_or(0),
        ))
    }

    /// Whether this version is at least the given one. Used like `semver.satisfies(coerce(v),
    /// ">=x.y.z")` in Ledger Live.
    pub fn at_least(&self, other: &SemVer) -> bool {
        self >= other
    }
}

fn parse_numeric(s: &str) -> Option<u64> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse().ok()
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                // A version without pre-release has a higher precedence.
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(other.pre.iter()) {
                        let ord = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(a), Ok(b)) => a.cmp(&b),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl std::fmt::Display for SemVer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

/// Whether `version`, once coerced, is at least `min`. Mirrors the
/// `versionSatisfies(semverCoerce(v) || v, ">=x.y.z")` pattern used across Ledger Live.
pub(crate) fn coerced_at_least(version: &str, min: (u64, u64, u64)) -> bool {
    SemVer::coerce(version)
        .map(|v| v.at_least(&SemVer::new(min.0, min.1, min.2)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strict() {
        assert_eq!(SemVer::parse("2.1.3"), Some(SemVer::new(2, 1, 3)));
        assert_eq!(
            SemVer::parse("1.2.3-rc.1").map(|v| v.pre),
            Some(vec!["rc".to_string(), "1".to_string()])
        );
        assert_eq!(SemVer::parse("1.2.3+abc"), Some(SemVer::new(1, 2, 3)));
        assert_eq!(SemVer::parse("1.2"), None);
        assert_eq!(SemVer::parse("1.2.3.4"), None);
        assert_eq!(SemVer::parse("01.2.3"), None);
        assert_eq!(SemVer::parse("a.b.c"), None);
        assert_eq!(SemVer::parse(""), None);
    }

    #[test]
    fn coerce_loose() {
        assert_eq!(SemVer::coerce("2.1.0-rc1"), Some(SemVer::new(2, 1, 0)));
        assert_eq!(SemVer::coerce("1.16"), Some(SemVer::new(1, 16, 0)));
        assert_eq!(SemVer::coerce("v3"), Some(SemVer::new(3, 0, 0)));
        assert_eq!(SemVer::coerce("1.0.3-osu"), Some(SemVer::new(1, 0, 3)));
        assert_eq!(SemVer::coerce("foo 1.2.3.4"), Some(SemVer::new(1, 2, 3)));
        assert_eq!(SemVer::coerce("none"), None);
        assert_eq!(SemVer::coerce(""), None);
    }

    #[test]
    fn ordering() {
        let v = |s| SemVer::parse(s).unwrap();
        assert!(v("2.1.0") > v("2.0.9"));
        assert!(v("2.1.0") > v("2.1.0-rc1"));
        assert!(v("2.1.0-rc.2") > v("2.1.0-rc.1"));
        assert!(v("2.1.0-rc.1") < v("2.1.0-rc.1.1"));
        assert!(v("2.1.0-alpha") > v("2.1.0-1"));
        assert_eq!(v("1.2.3").cmp(&v("1.2.3")), Ordering::Equal);
        assert!(coerced_at_least("2.1.0-lo1", (2, 1, 0)));
        assert!(!coerced_at_least("2.0.9", (2, 1, 0)));
        assert!(!coerced_at_least("", (0, 0, 0)));
    }
}
