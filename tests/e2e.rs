//! End-to-end tests with a local CAS, driving the `jj-xet` binary.

mod common;

use std::fs;

use common::BIG_SIZE;
use common::Env;
use common::big_contents;
use common::blob_at;
use common::dir_size;
use common::git;
use common::git_at_least;
use common::lfs_pointer;

#[test]
fn large_file_is_committed_as_pointer() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    let contents = big_contents(1);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    fs::write(repo.join("small.txt"), "hello\n").unwrap();
    env.jj(&repo, &["commit", "-m", "add files"]);

    let pointer = blob_at(&repo, "HEAD", "big.bin");
    assert_eq!(pointer, lfs_pointer(&contents));
    assert_eq!(blob_at(&repo, "HEAD", "small.txt"), "hello\n");
    assert_eq!(fs::read(repo.join("big.bin")).unwrap(), contents);
    assert!(dir_size(&repo.join(".git")) < BIG_SIZE as u64 / 10);

    let status = env.jj(&repo, &["xet", "status", "-r", "@-"]);
    assert!(status.contains("big.bin"), "{status}");
    assert!(!status.contains("small.txt"), "{status}");
    assert!(env.jj(&repo, &["diff", "--summary"]).is_empty());
}

#[test]
fn checkout_reconstructs_contents() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    let contents = big_contents(2);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    env.jj(&repo, &["commit", "-m", "add big"]);

    env.jj(&repo, &["new", "root()"]);
    assert!(!repo.join("big.bin").exists());
    env.jj(&repo, &["new", "description(substring:\"add big\")"]);
    assert_eq!(fs::read(repo.join("big.bin")).unwrap(), contents);
}

#[test]
fn edits_are_deduplicated() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    let mut contents = big_contents(3);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    env.jj(&repo, &["commit", "-m", "v1"]);
    let before = dir_size(&env.path("cas"));

    contents.extend_from_slice(b"appended");
    fs::write(repo.join("big.bin"), &contents).unwrap();
    env.jj(&repo, &["commit", "-m", "v2"]);
    let growth = dir_size(&env.path("cas")) - before;
    assert!(growth < BIG_SIZE as u64 / 10, "CAS grew by {growth} bytes");
}

#[test]
fn patterns_store_small_files_in_xet() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    env.jj(
        &repo,
        &[
            "config",
            "set",
            "--repo",
            "xet.patterns",
            "[\"*.safetensors\"]",
        ],
    );
    fs::write(repo.join("model.safetensors"), "tiny weights").unwrap();
    env.jj(&repo, &["commit", "-m", "add model"]);
    assert!(blob_at(&repo, "HEAD", "model.safetensors").contains("\nsize 12\n"));
    assert_eq!(
        fs::read_to_string(repo.join("model.safetensors")).unwrap(),
        "tiny weights"
    );
}

#[test]
fn git_clone_is_materialized_by_xet_init() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    let contents = big_contents(4);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    env.jj(&repo, &["commit", "-m", "add big"]);
    env.jj(&repo, &["bookmark", "create", "main", "-r", "@-"]);

    // Pushing goes through the stock git backend and transfers the pointer.
    git(env.dir.path(), &["init", "-q", "--bare", "remote.git"]);
    let remote = env.path("remote.git");
    env.jj(
        &repo,
        &["git", "remote", "add", "origin", remote.to_str().unwrap()],
    );
    env.jj(&repo, &["git", "push", "--bookmark", "main"]);
    assert!(dir_size(&remote) < BIG_SIZE as u64 / 10);

    git(
        env.dir.path(),
        &["clone", "-q", "-b", "main", "remote.git", "clone"],
    );
    let clone = env.path("clone");
    assert!(
        fs::read_to_string(clone.join("big.bin"))
            .unwrap()
            .starts_with("version https://git-lfs")
    );
    env.jj(&clone, &["git", "init", "--colocate"]);
    env.jj(&clone, &["xet", "init"]);
    assert_eq!(fs::read(clone.join("big.bin")).unwrap(), contents);
    assert!(env.jj(&clone, &["diff", "--summary"]).is_empty());
}

#[test]
fn missing_contents_leave_pointer() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    fs::write(repo.join("big.bin"), big_contents(5)).unwrap();
    env.jj(&repo, &["commit", "-m", "add big"]);
    let pointer = blob_at(&repo, "HEAD", "big.bin");

    git(env.dir.path(), &["clone", "-q", "repo", "clone"]);
    let clone = env.path("clone");
    env.jj(
        &clone,
        &[
            "git",
            "init",
            "--colocate",
            "--config",
            "xet.auto-init=false",
        ],
    );
    let output = env.jj_with(
        &clone,
        &[("JJ_XET_CAS", env.path("empty-cas"))],
        &["xet", "init"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("leaving the pointer file"), "{stderr}");
    assert_eq!(fs::read_to_string(clone.join("big.bin")).unwrap(), pointer);
    // The pointer snapshots back to itself.
    let diff = env.jj_with(
        &clone,
        &[("JJ_XET_CAS", env.path("empty-cas"))],
        &["diff", "--summary"],
    );
    assert!(diff.stdout.is_empty());

    // Once the contents are reachable, `xet init` fetches them.
    env.jj(&clone, &["xet", "init"]);
    assert_eq!(fs::read(clone.join("big.bin")).unwrap(), big_contents(5));
}

#[test]
fn conflicts_on_xet_files_stay_conflicted() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    env.jj(
        &repo,
        &["config", "set", "--repo", "xet.patterns", "[\"*.dat\"]"],
    );
    fs::write(repo.join("f.dat"), "base\n").unwrap();
    env.jj(&repo, &["commit", "-m", "base"]);
    fs::write(repo.join("f.dat"), "left\n").unwrap();
    env.jj(&repo, &["commit", "-m", "left"]);
    env.jj(&repo, &["new", "description(substring:base)"]);
    fs::write(repo.join("f.dat"), "right\n").unwrap();
    env.jj(&repo, &["commit", "-m", "right"]);
    env.jj(
        &repo,
        &[
            "new",
            "description(substring:left)",
            "description(substring:right)",
        ],
    );
    // Snapshotting the untouched conflict markers must not resolve it.
    for _ in 0..2 {
        env.jj(&repo, &["status"]);
        let conflict = env.jj(
            &repo,
            &[
                "log",
                "-r",
                "@",
                "--no-graph",
                "-T",
                "if(conflict, \"yes\", \"no\")",
            ],
        );
        assert_eq!(conflict, "yes");
    }
}

#[test]
fn concurrent_processes_share_the_local_cas() {
    let env = Env::new();
    let repos: Vec<_> = (0..3u64)
        .map(|i| {
            let repo = env.init_repo(&format!("repo{i}"));
            fs::write(repo.join("big.bin"), big_contents(20 + i)).unwrap();
            repo
        })
        .collect();
    std::thread::scope(|scope| {
        for repo in &repos {
            let env = &env;
            scope.spawn(move || env.jj(repo, &["status"]));
        }
    });
    for (i, repo) in repos.iter().enumerate() {
        env.jj(repo, &["commit", "-m", "add big"]);
        assert_eq!(
            blob_at(repo, "HEAD", "big.bin"),
            lfs_pointer(&big_contents(20 + i as u64))
        );
    }
}

#[test]
fn unfetchable_pointer_is_left_byte_identical_despite_eol_conversion() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    fs::write(repo.join("big.bin"), big_contents(30)).unwrap();
    env.jj(&repo, &["commit", "-m", "add big"]);
    let pointer = blob_at(&repo, "HEAD", "big.bin");

    git(env.dir.path(), &["clone", "-q", "repo", "clone"]);
    let clone = env.path("clone");
    env.jj(&clone, &["git", "init", "--colocate"]);
    env.jj(
        &clone,
        &[
            "config",
            "set",
            "--repo",
            "working-copy.eol-conversion",
            "input-output",
        ],
    );
    let no_cas = [("JJ_XET_CAS", env.path("empty-cas"))];
    env.jj_with(&clone, &no_cas, &["xet", "init"]);
    // Check out again through the Xet working copy while the CAS is missing.
    env.jj_with(&clone, &no_cas, &["new", "root()"]);
    env.jj_with(
        &clone,
        &no_cas,
        &["new", "description(substring:\"add big\")"],
    );
    assert_eq!(fs::read_to_string(clone.join("big.bin")).unwrap(), pointer);
    // So a later retry recognizes and fetches it.
    env.jj(&clone, &["xet", "init"]);
    assert_eq!(fs::read(clone.join("big.bin")).unwrap(), big_contents(30));
}

#[test]
fn pointer_text_below_the_threshold_is_not_smudged() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    env.jj(
        &repo,
        &["config", "set", "--repo", "xet.patterns", "[\"*.dat\"]"],
    );
    // `small.dat` goes to Xet by pattern. `fixture.txt` holds the same
    // pointer text, but snapshotting it would never produce a pointer, so it
    // must be checked out as the text it is.
    fs::write(repo.join("small.dat"), "small contents").unwrap();
    let pointer = lfs_pointer(b"small contents");
    fs::write(repo.join("fixture.txt"), &pointer).unwrap();
    env.jj(&repo, &["commit", "-m", "add files"]);
    env.jj(&repo, &["new", "root()"]);
    env.jj(&repo, &["new", "description(substring:\"add files\")"]);
    assert_eq!(
        fs::read_to_string(repo.join("fixture.txt")).unwrap(),
        pointer
    );
    assert_eq!(
        fs::read_to_string(repo.join("small.dat")).unwrap(),
        "small contents"
    );
    assert!(env.jj(&repo, &["diff", "--summary"]).is_empty());
}

#[test]
fn smudged_look_alike_pointers_snapshot_back_unchanged() {
    // Pointer text for large contents is indistinguishable from our own
    // pointers (by design, for git-lfs interop) and gets smudged. What must
    // hold is that the tree is unchanged by the round trip.
    let env = Env::new();
    let repo = env.init_repo("repo");
    let contents = big_contents(40);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    fs::write(repo.join("look-alike.txt"), lfs_pointer(&contents)).unwrap();
    env.jj(&repo, &["commit", "-m", "add files"]);
    env.jj(&repo, &["new", "root()"]);
    env.jj(&repo, &["new", "description(substring:\"add files\")"]);
    assert!(env.jj(&repo, &["diff", "--summary"]).is_empty());
    assert_eq!(
        blob_at(&repo, "HEAD", "look-alike.txt"),
        lfs_pointer(&contents)
    );
}

#[test]
fn relative_cas_paths_are_rejected() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    for (vars, args) in [
        (
            vec![],
            vec!["--config", "xet.endpoint=relative/cas", "status"],
        ),
        (vec![("JJ_XET_CAS", "relative/cas".into())], vec!["status"]),
    ] {
        let output = env.jj_output(&repo, &vars, &args);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("absolute"), "{stderr}");
    }
}

#[test]
fn xet_init_with_invalid_config_leaves_workspace_unconverted() {
    let env = Env::new();
    env.jj(
        env.dir.path(),
        &[
            "git",
            "init",
            "--colocate",
            "--config",
            "xet.auto-init=false",
            "repo",
        ],
    );
    let repo = env.path("repo");
    let type_path = repo.join(".jj/working_copy/type");
    let output = env.jj_output(
        &repo,
        &[],
        &["--config", "xet.patterns=[\"[\"]", "xet", "init"],
    );
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&type_path).unwrap(), "local");
    env.jj(&repo, &["xet", "init"]);
    assert_eq!(fs::read_to_string(&type_path).unwrap(), "xet");
}

#[test]
fn new_workspaces_are_converted_automatically() {
    let env = Env::new();
    let wc_type =
        |root: &std::path::Path| fs::read_to_string(root.join(".jj/working_copy/type")).unwrap();
    env.jj(env.dir.path(), &["git", "init", "--colocate", "repo"]);
    let repo = env.path("repo");
    assert_eq!(wc_type(&repo), "xet");
    let contents = big_contents(50);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    env.jj(&repo, &["commit", "-m", "add big"]);
    env.jj(&repo, &["bookmark", "create", "main", "-r", "@-"]);

    // jj's `workspace add` creates a git worktree, which needs git >= 2.42.
    if git_at_least(2, 42) {
        env.jj(&repo, &["workspace", "add", "../second"]);
        assert_eq!(wc_type(&env.path("second")), "xet");
    } else {
        eprintln!("skipping `jj workspace add`: needs git >= 2.42");
    }

    // A plain git clone adopted with `jj git init` fetches the contents.
    git(env.dir.path(), &["clone", "-q", "repo", "clone"]);
    let clone = env.path("clone");
    env.jj(&clone, &["git", "init", "--colocate"]);
    assert_eq!(wc_type(&clone), "xet");
    assert_eq!(fs::read(clone.join("big.bin")).unwrap(), contents);

    env.jj(
        env.dir.path(),
        &["git", "init", "--config", "xet.auto-init=false", "plain"],
    );
    assert_eq!(wc_type(&env.path("plain")), "local");
}

#[test]
fn one_snapshot_is_one_upload() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    for i in 0..3u64 {
        fs::write(repo.join(format!("f{i}.bin")), big_contents(60 + i)).unwrap();
    }
    env.jj(&repo, &["status"]);
    let shards = fs::read_dir(env.path("cas/xet/xorbs/shards"))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "mdb")
        })
        .count();
    assert_eq!(shards, 1);
    env.jj(&repo, &["commit", "-m", "add files"]);
    for i in 0..3u64 {
        assert_eq!(
            blob_at(&repo, "HEAD", &format!("f{i}.bin")),
            lfs_pointer(&big_contents(60 + i))
        );
    }
}

#[test]
fn empty_files_are_never_pointers() {
    let env = Env::new();
    let repo = env.init_repo("repo");
    env.jj(
        &repo,
        &["config", "set", "--repo", "xet.patterns", "[\"*.bin\"]"],
    );
    fs::write(repo.join("empty.bin"), "").unwrap();
    env.jj(&repo, &["commit", "-m", "add empty"]);
    assert_eq!(blob_at(&repo, "HEAD", "empty.bin"), "");
    env.jj(&repo, &["new", "root()"]);
    env.jj(&repo, &["new", "description(substring:\"add empty\")"]);
    assert_eq!(fs::read(repo.join("empty.bin")).unwrap(), b"");
}

#[test]
fn jj_git_clone_converts_and_fetches() {
    // jj's clone runs `git fetch`, which needs git >= 2.42.
    if !git_at_least(2, 42) {
        eprintln!("skipping: needs git >= 2.42");
        return;
    }
    let env = Env::new();
    let repo = env.init_repo("repo");
    let contents = big_contents(70);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    env.jj(&repo, &["commit", "-m", "add big"]);
    env.jj(&repo, &["bookmark", "create", "main", "-r", "@-"]);
    git(
        env.dir.path(),
        &["clone", "-q", "--bare", "repo", "origin.git"],
    );
    // A source without a slash: like jj, the destination ("origin") is
    // derived from the source's absolute form.
    env.jj(env.dir.path(), &["git", "clone", "origin.git"]);
    let clone = env.path("origin");
    assert_eq!(
        fs::read_to_string(clone.join(".jj/working_copy/type")).unwrap(),
        "xet"
    );
    assert_eq!(fs::read(clone.join("big.bin")).unwrap(), contents);
}
