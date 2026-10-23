//! Harness shared by the integration tests.
//!
//! The tests need a `git` binary on `PATH` for inspecting and cloning repos.

#![allow(dead_code)] // each test binary uses a different subset

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use sha2::Digest as _;
use tempfile::TempDir;

pub const BIG_SIZE: usize = 5_000_000;

pub struct Env {
    pub dir: TempDir,
}

impl Env {
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("config.toml"),
            "[user]\nname = \"Test\"\nemail = \"test@example.com\"\n",
        )
        .unwrap();
        Self { dir }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Runs jj without checking the exit status. All caches live in the
    /// temporary directory; `vars` override the defaults, e.g. to simulate
    /// another machine with its own cache.
    pub fn jj_output(&self, cwd: &Path, vars: &[(&str, PathBuf)], args: &[&str]) -> Output {
        let defaults: [(&str, OsString); 4] = [
            ("JJ_CONFIG", self.path("config.toml").into()),
            ("JJ_XET_CAS", self.path("cas").into()),
            ("JJ_XET_CACHE", self.path("cache").into()),
            ("HF_HOME", self.path("hf").into()),
        ];
        let mut command = Command::new(env!("CARGO_BIN_EXE_jj-xet"));
        command
            .args(args)
            .current_dir(cwd)
            .env_remove("JJ_USER")
            .env_remove("JJ_EMAIL")
            .envs(defaults)
            .envs(vars.iter().map(|(key, value)| (key, value)));
        command.output().unwrap()
    }

    pub fn jj_with(&self, cwd: &Path, vars: &[(&str, PathBuf)], args: &[&str]) -> Output {
        let output = self.jj_output(cwd, vars, args);
        assert!(
            output.status.success(),
            "jj {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    pub fn jj(&self, cwd: &Path, args: &[&str]) -> String {
        String::from_utf8(self.jj_with(cwd, &[], args).stdout).unwrap()
    }

    pub fn init_repo(&self, name: &str) -> PathBuf {
        self.jj(self.dir.path(), &["git", "init", "--colocate", name]);
        let repo = self.path(name);
        self.jj(&repo, &["xet", "init"]);
        repo
    }
}

pub fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Deterministic, incompressible-enough content.
pub fn big_contents(seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..BIG_SIZE)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

/// The canonical Git LFS pointer, as git-lfs itself would write it.
pub fn lfs_pointer(contents: &[u8]) -> String {
    let oid: String = sha2::Sha256::digest(contents)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        contents.len()
    )
}

pub fn dir_size(path: &Path) -> u64 {
    fs::read_dir(path)
        .map(|entries| {
            entries
                .map(|entry| {
                    let entry = entry.unwrap();
                    let meta = entry.metadata().unwrap();
                    if meta.is_dir() {
                        dir_size(&entry.path())
                    } else {
                        meta.len()
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

pub fn blob_at(repo: &Path, rev: &str, file: &str) -> String {
    git(repo, &["cat-file", "-p", &format!("{rev}:{file}")])
}

/// Whether the `git` on `PATH` is at least `major.minor`.
pub fn git_at_least(major: u32, minor: u32) -> bool {
    let version = git(Path::new("."), &["--version"]);
    let mut numbers = version
        .split_whitespace()
        .nth(2)
        .unwrap_or_default()
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0));
    let found = (numbers.next().unwrap_or(0), numbers.next().unwrap_or(0));
    found >= (major, minor)
}
