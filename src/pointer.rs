//! Git LFS pointer files.
//!
//! See <https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md>. Only the
//! canonical encoding is accepted: a pointer has exactly one valid encoding,
//! so the same content always yields the same blob as git-lfs, git-xet, `hf`
//! and openxet produce. Anything else, including pointers with extension
//! keys, is treated as a regular file and so preserved unchanged.

use std::fmt;

const VERSION_LINE: &str = "version https://git-lfs.github.com/spec/v1\n";

/// Pointer files are always smaller than this.
pub const MAX_POINTER_SIZE: usize = 1024;

/// A lowercase hex SHA-256 digest.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Sha256Hex(String);

impl Sha256Hex {
    pub fn parse(hex: &str) -> Option<Self> {
        let valid = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        valid.then(|| Self(hex.to_owned()))
    }

    pub fn from_digest(digest: &[u8]) -> Self {
        Self(digest.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pointer {
    pub oid: Sha256Hex,
    pub size: u64,
}

impl Pointer {
    pub fn to_bytes(&self) -> Vec<u8> {
        format!(
            "{VERSION_LINE}oid sha256:{}\nsize {}\n",
            self.oid, self.size
        )
        .into_bytes()
    }

    /// Parses a canonically encoded pointer.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= MAX_POINTER_SIZE {
            return None;
        }
        let rest = std::str::from_utf8(bytes)
            .ok()?
            .strip_prefix(VERSION_LINE)?;
        let (oid_line, size_line) = rest.strip_suffix('\n')?.split_once('\n')?;
        let oid = Sha256Hex::parse(oid_line.strip_prefix("oid sha256:")?)?;
        let size = size_line.strip_prefix("size ")?.parse().ok()?;
        let pointer = Self { oid, size };
        // Rejects non-canonical sizes such as "+1" or "01".
        (pointer.to_bytes() == bytes).then_some(pointer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";

    fn spec_example() -> String {
        format!("{VERSION_LINE}oid sha256:{OID}\nsize 12345\n")
    }

    #[test]
    fn parses_spec_example() {
        let pointer = Pointer::parse(spec_example().as_bytes()).unwrap();
        assert_eq!(pointer.oid.as_str(), OID);
        assert_eq!(pointer.size, 12345);
        assert_eq!(pointer.to_bytes(), spec_example().into_bytes());
    }

    #[test]
    fn rejects_non_canonical_encodings() {
        let cases = [
            // extension keys are valid LFS but must be preserved as-is
            format!("{VERSION_LINE}ext-0-foo sha256:{OID}\noid sha256:{OID}\nsize 1\n"),
            // our earlier format with an extra key
            format!("{VERSION_LINE}oid sha256:{OID}\nsize 1\nxet-hash {OID}\n"),
            format!("{VERSION_LINE}oid sha256:{}\nsize 1\n", OID.to_uppercase()),
            format!("{VERSION_LINE}oid sha256:{OID}\nsize 01\n"),
            format!("{VERSION_LINE}oid sha256:{OID}\nsize +1\n"),
            format!("{VERSION_LINE}oid sha256:{OID}\nsize 1"),
            format!("{VERSION_LINE}oid sha256:{OID}\r\nsize 1\r\n"),
            format!("{VERSION_LINE}size 1\noid sha256:{OID}\n"),
            "hello\n".to_owned(),
        ];
        for case in cases {
            assert_eq!(Pointer::parse(case.as_bytes()), None, "{case:?}");
        }
    }

    #[test]
    fn rejects_oversized_input() {
        let mut bytes = spec_example().into_bytes();
        bytes.resize(MAX_POINTER_SIZE, b'\n');
        assert_eq!(Pointer::parse(&bytes), None);
    }
}
