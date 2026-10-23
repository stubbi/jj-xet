//! Maps LFS oids (SHA-256) to Xet file hashes.
//!
//! Pointers only carry the SHA-256, but Xet reconstructs files by their Xet
//! hash. Both are pure functions of the content, so the map can be shared
//! freely between repos and servers. Entries come from our own uploads or
//! from a server lookup.

use std::fs;
use std::io;
use std::io::Write as _;
use std::path::PathBuf;

use jj_lib::file_util::persist_temp_file;
use tempfile::NamedTempFile;

use crate::pointer::Sha256Hex;

#[derive(Debug)]
pub struct OidMap {
    dir: PathBuf,
}

impl OidMap {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn entry_path(&self, oid: &Sha256Hex) -> PathBuf {
        self.dir.join(&oid.as_str()[..2]).join(oid.as_str())
    }

    pub fn get(&self, oid: &Sha256Hex) -> io::Result<Option<String>> {
        match fs::read_to_string(self.entry_path(oid)) {
            // Xet hashes have the same shape as SHA-256 hex digests.
            Ok(contents) => match Sha256Hex::parse(contents.trim_end()) {
                Some(hash) => Ok(Some(hash.as_str().to_owned())),
                None => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("corrupt oid map entry for {oid}"),
                )),
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn remove(&self, oid: &Sha256Hex) -> io::Result<()> {
        match fs::remove_file(self.entry_path(oid)) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }

    pub fn insert(&self, oid: &Sha256Hex, xet_hash: &str) -> io::Result<()> {
        let path = self.entry_path(oid);
        let dir = path.parent().expect("entry path has a parent");
        fs::create_dir_all(dir)?;
        // Write atomically so concurrent snapshots never see a partial entry.
        let mut temp_file = NamedTempFile::new_in(dir)?;
        writeln!(temp_file, "{xet_hash}")?;
        persist_temp_file(temp_file, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_misses() {
        let dir = tempfile::tempdir().unwrap();
        let map = OidMap::new(dir.path().to_owned());
        let oid = Sha256Hex::parse(&"a".repeat(64)).unwrap();
        assert_eq!(map.get(&oid).unwrap(), None);
        map.insert(&oid, &"b".repeat(64)).unwrap();
        map.insert(&oid, &"b".repeat(64)).unwrap();
        assert_eq!(map.get(&oid).unwrap(), Some("b".repeat(64)));
        map.remove(&oid).unwrap();
        map.remove(&oid).unwrap();
        assert_eq!(map.get(&oid).unwrap(), None);
    }

    #[test]
    fn reports_corrupt_entries() {
        let dir = tempfile::tempdir().unwrap();
        let map = OidMap::new(dir.path().to_owned());
        let oid = Sha256Hex::parse(&"a".repeat(64)).unwrap();
        map.insert(&oid, "not a hash").unwrap();
        assert!(map.get(&oid).is_err());
    }
}
