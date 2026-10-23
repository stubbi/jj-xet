//! Clean/smudge filter that moves large file contents into Xet storage.
//!
//! On snapshot, a large file is chunked and deduplicated into a Xet CAS and
//! the jj tree only records a standard Git LFS pointer. On checkout, pointers
//! are reconstructed from the CAS and verified against their SHA-256.
//!
//! The CAS is either a local directory, possibly shared, or an
//! openxet-compatible server reached through a Git LFS URL.

use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use globset::Glob;
use globset::GlobSet;
use globset::GlobSetBuilder;
use jj_lib::config::ConfigGetError;
use jj_lib::config::ConfigGetResultExt as _;
use jj_lib::file_util::expand_home_path;
use jj_lib::lock::FileLock;
use jj_lib::repo_path::RepoPath;
use jj_lib::settings::UserSettings;
use once_cell::sync::OnceCell;
use sha2::Digest as _;
use sha2::Sha256;
use xet_data::processing::FileDownloadSession;
use xet_data::processing::FileUploadSession;
use xet_data::processing::Sha256Policy;
use xet_data::processing::XetFileInfo;
use xet_data::processing::configurations::TranslatorConfig;
use xet_data::processing::data_client;
use xet_runtime::core::XetContext;

use crate::oid_map::OidMap;
use crate::pointer::Pointer;
use crate::pointer::Sha256Hex;
use crate::remote::Access;
use crate::remote::LfsRemote;
use crate::remote::Refresher;

const DEFAULT_MIN_SIZE: u64 = 1024 * 1024;

/// Process-wide Xet context. It owns a tokio pool, since jj itself drives
/// futures with `pollster` and never runs inside tokio.
fn context() -> io::Result<&'static XetContext> {
    static CONTEXT: OnceCell<XetContext> = OnceCell::new();
    // Initialized at most once even when snapshot threads race here; each
    // context owns a thread pool.
    CONTEXT.get_or_try_init(|| XetContext::default().map_err(io::Error::other))
}

/// Runs a future on the Xet runtime from a non-tokio thread.
fn block_on<F, T, E>(future: F) -> io::Result<T>
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    context()?
        .runtime
        .bridge_sync(future)
        .map_err(io::Error::other)?
        .map_err(io::Error::other)
}

/// Per-user cache, `$JJ_XET_CACHE` or `<platform cache dir>/jj-xet`.
fn user_cache_dir() -> PathBuf {
    std::env::var_os("JJ_XET_CACHE").map_or_else(
        || {
            dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("jj-xet")
        },
        PathBuf::from,
    )
}

/// Parses a local CAS location. It must be absolute: jj may run from any
/// subdirectory of the workspace, and a relative CAS would end up inside it.
fn local_cas_dir(name: &str, value: &str) -> Result<PathBuf, ConfigGetError> {
    let value = value.strip_prefix("local://").unwrap_or(value);
    if value.contains("://") {
        return Err(invalid_config(
            name,
            "must be a directory; use `xet.lfs-url` for servers",
        ));
    }
    let path = expand_home_path(value, dirs::home_dir().as_deref());
    if !path.is_absolute() {
        return Err(invalid_config(
            name,
            format!("must be an absolute path, got {value:?}"),
        ));
    }
    Ok(path)
}

#[derive(Debug)]
enum Backend {
    Local { cas_dir: PathBuf },
    Remote(Arc<LfsRemote>),
}

/// Settings and Xet session config for one working copy.
pub struct XetFilter {
    backend: Backend,
    min_size: u64,
    patterns: GlobSet,
    oids: OidMap,
    upload_config: OnceCell<Arc<TranslatorConfig>>,
    download_config: OnceCell<Arc<TranslatorConfig>>,
    batch: Mutex<BatchState>,
}

impl std::fmt::Debug for XetFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The translator configs hold CAS tokens, so they are left out.
        f.debug_struct("XetFilter")
            .field("backend", &self.backend)
            .field("min_size", &self.min_size)
            .field("patterns", &self.patterns)
            .finish_non_exhaustive()
    }
}

/// An upload session shared by all files cleaned during one snapshot, so a
/// snapshot is one shard upload and its files deduplicate against each other.
struct UploadBatch {
    session: Arc<FileUploadSession>,
    /// Held for the batch's lifetime when the CAS is local.
    _lock: Option<FileLock>,
}

#[derive(Default)]
enum BatchState {
    #[default]
    Off,
    /// Batching; the session is created by the first upload.
    Open(Option<UploadBatch>),
}

impl std::fmt::Debug for BatchState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "Off",
            Self::Open(None) => "Open(idle)",
            Self::Open(Some(_)) => "Open(active)",
        })
    }
}

/// Ends an upload batch. Dropping it without [`Self::finish`] discards the
/// batch, e.g. when the snapshot failed and its pointers are never recorded.
#[must_use]
pub struct UploadBatchGuard {
    filter: Arc<XetFilter>,
}

impl UploadBatchGuard {
    /// Completes the uploads, so the batch's pointers can be committed.
    pub fn finish(self) -> io::Result<()> {
        let state = std::mem::take(&mut *self.filter.batch.lock().unwrap());
        if let BatchState::Open(Some(batch)) = state {
            let session = batch.session.clone();
            block_on(async move { session.finalize().await })?;
            drop(batch);
        }
        Ok(())
    }
}

impl Drop for UploadBatchGuard {
    fn drop(&mut self) {
        *self.filter.batch.lock().unwrap() = BatchState::Off;
    }
}

fn invalid_config(
    name: &str,
    error: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> ConfigGetError {
    ConfigGetError::Type {
        name: name.to_owned(),
        error: error.into(),
        source_path: None,
    }
}

impl XetFilter {
    /// Reads the `xet.*` config table. See the README for the keys.
    pub fn from_settings(settings: &UserSettings) -> Result<Self, ConfigGetError> {
        let lfs_url = settings.get_string("xet.lfs-url").optional()?;
        let local_dir = settings.get_string("xet.endpoint").optional()?;
        let token = settings.get_string("xet.token").optional()?;
        let backend = match (lfs_url, local_dir) {
            (Some(_), Some(_)) => {
                return Err(invalid_config(
                    "xet.lfs-url",
                    "set either `xet.lfs-url` or `xet.endpoint`, not both",
                ));
            }
            (Some(url), None) => Backend::Remote(Arc::new(
                LfsRemote::new(&url, token).map_err(|err| invalid_config("xet.lfs-url", err))?,
            )),
            (None, local_dir) => {
                if token.is_some() {
                    return Err(invalid_config(
                        "xet.token",
                        "`xet.token` only applies to `xet.lfs-url`",
                    ));
                }
                let cas_dir = match (local_dir, std::env::var("JJ_XET_CAS").ok()) {
                    (Some(dir), _) => local_cas_dir("xet.endpoint", &dir)?,
                    (None, Some(dir)) => local_cas_dir("JJ_XET_CAS", &dir)?,
                    (None, None) => user_cache_dir().join("cas"),
                };
                Backend::Local { cas_dir }
            }
        };
        let min_size = match settings.get_int("xet.min-size").optional()? {
            None => DEFAULT_MIN_SIZE,
            Some(size) if size >= 1 => size as u64,
            Some(size) => {
                return Err(invalid_config(
                    "xet.min-size",
                    format!("must be at least 1, got {size}"),
                ));
            }
        };
        let mut builder = GlobSetBuilder::new();
        for pattern in settings
            .get::<Vec<String>>("xet.patterns")
            .optional()?
            .unwrap_or_default()
        {
            builder.add(Glob::new(&pattern).map_err(|err| invalid_config("xet.patterns", err))?);
        }
        let patterns = builder
            .build()
            .map_err(|err| invalid_config("xet.patterns", err))?;
        // A shared local CAS carries its own map so everyone sharing it can
        // resolve pointers. For a server, the map is only a cache.
        let oids = OidMap::new(match &backend {
            Backend::Local { cas_dir } => cas_dir.join("oids"),
            Backend::Remote(_) => user_cache_dir().join("oids"),
        });
        Ok(Self {
            backend,
            min_size,
            patterns,
            oids,
            upload_config: OnceCell::new(),
            download_config: OnceCell::new(),
            batch: Mutex::default(),
        })
    }

    /// Where contents are stored, for display.
    pub fn location(&self) -> String {
        match &self.backend {
            Backend::Local { cas_dir } => cas_dir.display().to_string(),
            Backend::Remote(remote) => remote.url().to_string(),
        }
    }

    fn translator_config(&self, access: Access) -> io::Result<Arc<TranslatorConfig>> {
        let slot = match access {
            Access::Read => &self.download_config,
            Access::Write => &self.upload_config,
        };
        slot.get_or_try_init(|| {
            let config = match &self.backend {
                Backend::Local { cas_dir } => data_client::default_config(
                    context()?,
                    format!("local://{}", cas_dir.display()),
                    None,
                    None,
                    None,
                ),
                Backend::Remote(remote) => {
                    let token = block_on({
                        let remote = remote.clone();
                        async move { remote.cas_token(access).await }
                    })?;
                    let refresher = Arc::new(Refresher {
                        remote: remote.clone(),
                        access,
                    });
                    data_client::default_config(
                        context()?,
                        token.cas_url,
                        Some((token.access_token, token.exp)),
                        Some(refresher),
                        None,
                    )
                }
            };
            Ok(Arc::new(config.map_err(io::Error::other)?))
        })
        .cloned()
    }

    /// A local CAS is a single-writer database, so every session against it
    /// holds a lock, across threads and processes.
    fn lock_local_cas(&self) -> io::Result<Option<FileLock>> {
        let Backend::Local { cas_dir } = &self.backend else {
            return Ok(None);
        };
        std::fs::create_dir_all(cas_dir)?;
        FileLock::lock(cas_dir.join("jj-xet.lock"))
            .map(Some)
            .map_err(io::Error::other)
    }

    /// Whether a file of this size at this path is stored in Xet.
    pub fn should_clean(&self, path: &RepoPath, size: u64) -> bool {
        // Like git-lfs, empty files are never replaced by pointers.
        size > 0
            && (size >= self.min_size || self.patterns.is_match(path.as_internal_file_string()))
    }

    fn pointer_for(&self, info: &XetFileInfo) -> io::Result<Pointer> {
        let oid = info
            .sha256()
            .and_then(Sha256Hex::parse)
            .ok_or_else(|| io::Error::other("Xet did not report a SHA-256"))?;
        let size = info
            .file_size()
            .ok_or_else(|| io::Error::other("Xet did not report the file size"))?;
        self.oids.insert(&oid, info.hash())?;
        Ok(Pointer { oid, size })
    }

    /// Starts batching uploads until the guard is finished or dropped.
    pub fn begin_upload_batch(self: &Arc<Self>) -> UploadBatchGuard {
        *self.batch.lock().unwrap() = BatchState::Open(None);
        UploadBatchGuard {
            filter: self.clone(),
        }
    }

    /// The open batch's session, created on first use, or `None` when not
    /// batching.
    fn batch_session(&self) -> io::Result<Option<Arc<FileUploadSession>>> {
        let mut state = self.batch.lock().unwrap();
        let BatchState::Open(slot) = &mut *state else {
            return Ok(None);
        };
        if let Some(batch) = slot {
            return Ok(Some(batch.session.clone()));
        }
        let config = self.translator_config(Access::Write)?;
        let lock = self.lock_local_cas()?;
        let session = block_on(async move { FileUploadSession::new(config).await })?;
        *slot = Some(UploadBatch {
            session: session.clone(),
            _lock: lock,
        });
        Ok(Some(session))
    }

    /// Uploads the file on disk and returns the pointer that replaces it.
    /// Within a batch, the upload completes when the batch is finished.
    pub fn clean_file(&self, disk_path: &Path) -> io::Result<Pointer> {
        let disk_path = disk_path.to_owned();
        let info = if let Some(session) = self.batch_session()? {
            block_on(async move {
                data_client::clean_file(session, &disk_path, Sha256Policy::Compute)
                    .await
                    .map(|(info, _metrics)| info)
            })?
        } else {
            let config = self.translator_config(Access::Write)?;
            let _lock = self.lock_local_cas()?;
            block_on(async move {
                let session = FileUploadSession::new(config).await?;
                let (info, _metrics) =
                    data_client::clean_file(session.clone(), &disk_path, Sha256Policy::Compute)
                        .await?;
                session.finalize().await?;
                Ok::<_, xet_data::DataError>(info)
            })?
        };
        self.pointer_for(&info)
    }

    /// The Xet hash for `pointer`, and whether it is new from the server, in
    /// which case it is only cached once a download verified it.
    fn xet_hash(&self, pointer: &Pointer) -> io::Result<(String, bool)> {
        if let Some(hash) = self.oids.get(&pointer.oid)? {
            return Ok((hash, false));
        }
        let hash = match &self.backend {
            Backend::Local { .. } => None,
            Backend::Remote(remote) => block_on({
                let remote = remote.clone();
                let pointer = pointer.clone();
                async move { remote.lookup_xet_hash(&pointer).await }
            })?,
        };
        let hash = hash.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} does not know object {}", self.location(), pointer.oid),
            )
        })?;
        Ok((hash, true))
    }

    /// Writes the contents of `pointer` into `file`, verifying size and
    /// SHA-256. On error, `file` may hold partial contents. Returns the
    /// number of bytes written.
    pub fn smudge_to_file(&self, pointer: &Pointer, file: &File) -> io::Result<u64> {
        let (xet_hash, from_server) = self.xet_hash(pointer)?;
        let info =
            XetFileInfo::new_with_sha256(xet_hash.clone(), pointer.size, pointer.oid.to_string());
        let config = self.translator_config(Access::Read)?;
        let hasher = Arc::new(Mutex::new(Sha256::new()));
        let writer = HashingWriter {
            inner: file.try_clone()?,
            hasher: hasher.clone(),
        };
        let _lock = self.lock_local_cas()?;
        let (_id, size) = block_on(async move {
            let session = FileDownloadSession::new(config, None).await?;
            session.download_to_writer(&info, .., writer).await
        })?;
        let digest = Sha256Hex::from_digest(&hasher.lock().unwrap().clone().finalize());
        if size != pointer.size || digest != pointer.oid {
            // A wrong mapping must not stick, or retries could never succeed.
            if !from_server {
                self.oids.remove(&pointer.oid)?;
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "reconstructed {size} bytes with SHA-256 {digest}, expected {} bytes with {}",
                    pointer.size, pointer.oid
                ),
            ));
        }
        if from_server {
            self.oids.insert(&pointer.oid, &xet_hash)?;
        }
        Ok(size)
    }
}

/// Hashes everything written through it.
struct HashingWriter {
    inner: File,
    hasher: Arc<Mutex<Sha256>>,
}

impl Write for HashingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.lock().unwrap().update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
