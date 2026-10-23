# jj-xet

[Jujutsu](https://github.com/jj-vcs/jj) with large files stored in
[Xet](https://github.com/huggingface/xet-core), Hugging Face's
content-defined-chunking storage.

jj has no Git LFS support, because it doesn't run `.gitattributes` filters.
`jj-xet` is the stock `jj` CLI plus an `xet` working copy type that acts as
that filter: on snapshot, large files are chunked and deduplicated into Xet,
and only a standard Git LFS pointer is committed. On checkout, pointers are
reconstructed and verified against their SHA-256.

```
$ jj xet init
Switched this workspace to the Xet working copy (storage: /Users/me/Library/Caches/jj-xet/cas)
$ head -c 5000000 /dev/urandom > big.bin
$ jj commit -m "add big.bin"
$ jj xet status -r @-
3f18a9675c06      5000000 big.bin
$ git cat-file -p @-:big.bin        # in a colocated repo
version https://git-lfs.github.com/spec/v1
oid sha256:3f18a9675c068311da2304421cfa7ad9b2b13b503aa1c76f89698127c7afce47
size 5000000
```

Appending a few bytes to that 5 MB file and committing again grows the
storage by about 80 KiB, because unchanged chunks are deduplicated.

## Install

```
cargo install --path .
```

This installs a `jj-xet` binary. It is a complete `jj`, so you can alias it
(`alias jj=jj-xet`).

## Usage

- `jj git init`, `jj git clone` and `jj workspace add` create workspaces
  that already use the Xet working copy (unless `xet.auto-init = false`).
- `jj xet init` switches an existing workspace to the Xet working copy. It
  also replaces pointer files on disk with their contents, so run it again to
  retry after contents become available.
- `jj xet status [-r REV]` lists the files stored in Xet in a revision.

Everything else is plain jj. `jj git push`/`fetch` work unchanged and
transfer pointer files only.

## Storage

**Local (default).** Contents go to a CAS directory shared by all repos of
the user on this machine. Concurrent jj processes take turns on it.

**openxet.** For a team, point the repo at an
[openxet](https://github.com/stubbi/openxet) Git LFS URL. This works for git
repos hosted anywhere:

```
jj config set --repo xet.lfs-url https://openxet.example/ns/repo.git/info/lfs
```

jj-xet gets short-lived CAS tokens from `{lfs-url}/xet-write-token` and
`{lfs-url}/xet-read-token`, and refreshes them before they expire. It
authenticates with an openxet access token, from `xet.token` or, like
git-lfs, from git's credential helpers (`git credential fill`). Because the
pointers are standard, the same repo works with git-lfs, git-xet, `hf` and
openxet's own client.

## Configuration

| Key | Default | Meaning |
|---|---|---|
| `xet.lfs-url` | unset | openxet Git LFS URL; stores contents on that server |
| `xet.token` | git credential helpers | openxet access token for `xet.lfs-url` |
| `xet.endpoint` | `$JJ_XET_CAS`, else `<user cache dir>/jj-xet/cas` | Absolute path (`~` allowed) of a local CAS directory |
| `xet.min-size` | `1048576` | Files with at least this many bytes go to Xet |
| `xet.patterns` | `[]` | Globs on repo paths that always go to Xet, e.g. `["*.safetensors", "data/**"]` |
| `xet.auto-init` | `true` | Use the Xet working copy for workspaces created by `jj git init`/`clone` and `jj workspace add` |

`$JJ_XET_CACHE` (default `<user cache dir>/jj-xet`) holds the oid map for
remote storage. Xet's own chunk cache follows `HF_HOME`/`HF_XET_CACHE`, so it
is shared with `hf`.

## Design

**Why a working copy and not a backend.** jj's commit backend is pluggable,
but `jj git push`, `fetch`, colocation and `gc` all require the backend's
concrete type to be `GitBackend`. A wrapping backend would break them. The
working copy is the layer where git runs its clean/smudge filters anyway, and
replacing it leaves the stock git backend untouched.

**Vendored working copy.** jj's local working copy has no content-filter hook,
so `src/vendor/local_working_copy.rs` is a copy of jj-lib 0.46.0's
(Apache-2.0) with small patches, all of them referencing `xet`:

- Snapshot: `clean_large_file` before a file is written to the store, and
  exempting Xet-bound files from `snapshot.max-new-file-size`. A conflicted
  file is only cleaned once it no longer parses as a conflict, so untouched
  conflict markers are never recorded as a resolution.
- Checkout: `write_file` smudges a pointer only if snapshotting that path
  would produce it, so pointer-like text below the threshold round-trips. It
  reconstructs into a temporary file in `.jj/working_copy/`, outside the
  tracked tree even if jj is interrupted, and renames it into place only
  after size and SHA-256 check out. If the contents can't be fetched, the pointer is
  written byte-identical, bypassing EOL conversion, so a later `jj xet init`
  can still fetch it.
- `materialize_xet_pointers` for `jj xet init`.
- Each snapshot is one Xet upload session, finished before the snapshot
  returns: one shard per snapshot, and files deduplicate against each other.

`src/vendor/xet.patch` holds these changes against the mechanically adapted
upstream file, and `scripts/vendor_jj.py` re-applies it to a new jj release.
CI checks that the patch matches the vendored code, and a daily workflow
(`.github/workflows/jj-release.yml`) opens a PR for each new jj release, or an
issue if the patch needs a human.

The on-disk state format is unchanged, so switching a workspace is just
rewriting `.jj/working_copy/type` from `local` to `xet`, under the working
copy lock. jj's change detection records the size on disk after conversion,
so an unchanged large file is never re-read.

**Pointers.** Empty files are never replaced by pointers, as with git-lfs.
Pointers are the canonical 3-key Git LFS pointer, and only that: a pointer
has exactly one valid encoding, so the same content always yields the same
blob as other LFS tools. Pointers with extension keys or any other encoding
are treated as regular files and preserved. Xet reconstructs files by their
Xet hash, not by SHA-256. The oid map (`src/oid_map.rs`) holds
SHA-256 → Xet hash entries from our own uploads. For openxet, misses are
looked up through the LFS batch API (`X-Xet-Hash`) and cached only after a
download verified them. Both hashes are pure functions of the content, so the
map can be shared freely. The `casUrl` a server hands out must be http(s):
xet picks its storage backend from the URL scheme.

## Testing

`cargo test` runs unit tests, end-to-end tests against a local CAS
(`tests/e2e.rs`), and tests of the openxet client contract against a mock
openxet in front of xet-core's HTTP CAS server (`tests/openxet.rs`). The
integration tests need `git` on `PATH`.

## Limitations

- **Uploads happen at snapshot time.** Snapshots wait for the upload, as with
  `git add` under git-xet's clean filter. A Xet-aware push could defer them,
  but jj has no push hook into the working copy.
- **The local CAS is single-machine**, on purpose. It is a database with file
  locks, so don't put it on a network mount. Use openxet to share. Concurrent
  jj processes take turns on it.
- **Diffs show pointers.** `jj diff` and `jj file show` show pointer text,
  like `git diff` on LFS files. Rendering contents would need a wrapping
  commit backend, which breaks `jj git` (see Design). Most large files are
  binary, which jj doesn't diff anyway.
- **Missing contents** leave the pointer on disk with a warning, like
  `GIT_LFS_SKIP_SMUDGE`. The pointer snapshots back to itself, so nothing is
  lost, and `jj xet init` fetches it later.
- **No garbage collection** of the local CAS. It is shared by all your repos,
  so no single repo knows what is unreferenced. openxet collects on the
  server.
- **openxet is not released yet.** The client follows the agreed contract and
  is tested against a mock.
- `xet.auto-init` runs `jj xet init` in a child process, which doesn't see
  `--config` arguments given to the original command. Use config files or
  `JJ_CONFIG` for settings that must apply to the new workspace.

## License

Apache-2.0. `src/vendor/` contains code from
[jj](https://github.com/jj-vcs/jj), Copyright the Jujutsu Authors,
also Apache-2.0.
