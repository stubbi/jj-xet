//! Tests against the openxet client contract, with a mock openxet in front of
//! xet-core's HTTP CAS server.
//!
//! The contract, per openxet: `GET {lfs-url}/xet-{read,write}-token` with
//! HTTP Basic auth (access token as password) returns
//! `{"accessToken", "exp" (unix seconds), "casUrl"}`; errors are
//! `{"schema": "openxet.error.v1", "code", "message", "hint"}`; and
//! `POST {lfs-url}/objects/batch` returns `X-Xet-Hash` in each object's
//! `actions.download.header`.

mod common;

use std::collections::HashMap;
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use base64::Engine as _;
use common::BIG_SIZE;
use common::Env;
use common::big_contents;
use common::blob_at;
use common::git;
use common::lfs_pointer;
use serde_json::Value;
use serde_json::json;
use tokio::runtime::Runtime;
use xet_client::cas_client::simulation::LocalServer;
use xet_client::cas_client::simulation::LocalServerConfig;
use xet_runtime::core::XetContext;

const LFS_PATH: &str = "/ns/repo.git/info/lfs";
const TOKEN: &str = "secret";

#[derive(Clone, Default)]
struct Mock {
    cas_url: Arc<Mutex<String>>,
    /// Lifetime of issued CAS tokens.
    token_ttl_secs: Arc<AtomicU64>,
    /// sha256 → xet hash, as openxet registers from uploaded shards.
    xet_hashes: Arc<Mutex<HashMap<String, String>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

fn openxet_error(status: StatusCode, code: &str, message: &str, hint: &str) -> Response {
    let body = json!({
        "schema": "openxet.error.v1",
        "code": code,
        "message": message,
        "hint": hint,
    });
    (status, Json(body)).into_response()
}

fn authorized(headers: &HeaderMap) -> bool {
    let Some(basic) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
    else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(basic) else {
        return false;
    };
    // The username is ignored, the password is the access token.
    String::from_utf8_lossy(&decoded)
        .split_once(':')
        .is_some_and(|(_user, password)| password == TOKEN)
}

async fn token(state: Mock, route: &str, headers: HeaderMap) -> Response {
    state.requests.lock().unwrap().push(route.to_owned());
    if !authorized(&headers) {
        return openxet_error(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "invalid access token",
            "run `openxet login`",
        );
    }
    let exp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + state.token_ttl_secs.load(Ordering::Relaxed);
    let cas_url = state.cas_url.lock().unwrap().clone();
    Json(json!({
        "accessToken": format!("cas-{route}"),
        "exp": exp,
        "casUrl": cas_url,
    }))
    .into_response()
}

async fn batch(State(state): State<Mock>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    state.requests.lock().unwrap().push("batch".to_owned());
    if !authorized(&headers) {
        return openxet_error(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "invalid access token",
            "",
        );
    }
    assert_eq!(body["operation"], "download");
    let hashes = state.xet_hashes.lock().unwrap();
    let objects: Vec<Value> = body["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|object| {
            let oid = object["oid"].as_str().unwrap();
            match hashes.get(oid) {
                Some(hash) => json!({
                    "oid": oid,
                    "size": object["size"],
                    "actions": {"download": {
                        "href": format!("/objects/{oid}"),
                        "header": {"X-Xet-Hash": hash},
                    }},
                }),
                None => json!({
                    "oid": oid,
                    "size": object["size"],
                    "error": {"code": 404, "message": "Object does not exist"},
                }),
            }
        })
        .collect();
    // ADR 0003: downloads are always "basic", for git-lfs with git-xet.
    Json(json!({"transfer": "basic", "objects": objects})).into_response()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Servers {
    runtime: Runtime,
    mock: Mock,
    lfs_url: String,
}

impl Servers {
    fn start(env: &Env) -> Self {
        let runtime = Runtime::new().unwrap();
        let cas_port = free_port();
        let cas_url = format!("http://127.0.0.1:{cas_port}");
        let data_directory = env.path("server-cas");
        runtime.block_on(async {
            let cas = LocalServer::new(
                XetContext::default().unwrap(),
                LocalServerConfig {
                    data_directory,
                    host: "127.0.0.1".to_owned(),
                    port: cas_port,
                    in_memory: false,
                },
            )
            .await
            .unwrap();
            let (_stop, stopped) = tokio::sync::oneshot::channel::<()>();
            tokio::spawn(async move {
                // Keep the sender alive for the runtime's lifetime.
                let _stop = _stop;
                cas.run_until_stopped(stopped).await.unwrap();
            });
        });
        let mock = Mock {
            cas_url: Arc::new(Mutex::new(cas_url.clone())),
            token_ttl_secs: Arc::new(AtomicU64::new(3600)),
            ..Mock::default()
        };
        let app = Router::new()
            .route(
                &format!("{LFS_PATH}/xet-write-token"),
                get(|State(state): State<Mock>, headers: HeaderMap| token(state, "write", headers)),
            )
            .route(
                &format!("{LFS_PATH}/xet-read-token"),
                get(|State(state): State<Mock>, headers: HeaderMap| token(state, "read", headers)),
            )
            .route(&format!("{LFS_PATH}/objects/batch"), post(batch))
            .with_state(mock.clone());
        let lfs_url = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            // Wait until the CAS accepts connections.
            for _ in 0..100 {
                if tokio::net::TcpStream::connect(("127.0.0.1", cas_port))
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            format!("http://{addr}{LFS_PATH}")
        });
        Self {
            runtime,
            mock,
            lfs_url,
        }
    }

    fn count(&self, request: &str) -> usize {
        self.requests().iter().filter(|r| *r == request).count()
    }

    fn requests(&self) -> Vec<String> {
        self.mock.requests.lock().unwrap().clone()
    }

    /// Registers the oid → xet hash entries a machine's uploads produced,
    /// which openxet learns from the uploaded shards.
    fn register_uploads(&self, cache_dir: &Path) {
        for shard in fs::read_dir(cache_dir.join("oids")).unwrap() {
            for entry in fs::read_dir(shard.unwrap().path()).unwrap() {
                let entry = entry.unwrap();
                let oid = entry.file_name().into_string().unwrap();
                let hash = fs::read_to_string(entry.path()).unwrap().trim().to_owned();
                self.mock.xet_hashes.lock().unwrap().insert(oid, hash);
            }
        }
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        // Don't wait for the servers' tasks.
        let runtime = std::mem::replace(&mut self.runtime, Runtime::new().unwrap());
        runtime.shutdown_background();
    }
}

fn use_openxet(env: &Env, repo: &Path, lfs_url: &str, token: Option<&str>) {
    env.jj(repo, &["config", "set", "--repo", "xet.lfs-url", lfs_url]);
    if let Some(token) = token {
        env.jj(repo, &["config", "set", "--repo", "xet.token", token]);
    }
}

/// Env vars for a second machine with its own caches and no local CAS.
fn other_machine(env: &Env) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("JJ_XET_CACHE", env.path("cache-b")),
        ("JJ_XET_CAS", env.path("no-cas")),
        ("HF_HOME", env.path("hf-b")),
    ]
}

#[test]
fn round_trip_through_openxet() {
    let env = Env::new();
    let servers = Servers::start(&env);
    let repo = env.init_repo("repo");
    use_openxet(&env, &repo, &servers.lfs_url, Some(TOKEN));

    let contents = big_contents(10);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    env.jj(&repo, &["commit", "-m", "add big"]);
    assert_eq!(blob_at(&repo, "HEAD", "big.bin"), lfs_pointer(&contents));
    assert!(servers.requests().contains(&"write".to_owned()));
    servers.register_uploads(&env.path("cache"));

    // Another machine clones with plain git; the contents come from openxet.
    git(env.dir.path(), &["clone", "-q", "repo", "clone"]);
    let clone = env.path("clone");
    let machine_b = other_machine(&env);
    env.jj_with(&clone, &machine_b, &["git", "init", "--colocate"]);
    env.jj_with(
        &clone,
        &machine_b,
        &["config", "set", "--repo", "xet.lfs-url", &servers.lfs_url],
    );
    env.jj_with(
        &clone,
        &machine_b,
        &["config", "set", "--repo", "xet.token", TOKEN],
    );
    env.jj_with(&clone, &machine_b, &["xet", "init"]);
    assert_eq!(fs::read(clone.join("big.bin")).unwrap(), contents);
    let requests = servers.requests();
    assert!(requests.contains(&"batch".to_owned()), "{requests:?}");
    assert!(requests.contains(&"read".to_owned()), "{requests:?}");
    let diff = env.jj_with(&clone, &machine_b, &["diff", "--summary"]);
    assert!(diff.stdout.is_empty());
}

#[test]
fn rejected_token_fails_snapshot_instead_of_committing_contents() {
    let env = Env::new();
    let servers = Servers::start(&env);
    let repo = env.init_repo("repo");
    use_openxet(&env, &repo, &servers.lfs_url, Some("wrong"));

    fs::write(repo.join("big.bin"), big_contents(11)).unwrap();
    let output = env.jj_output(&repo, &[], &["commit", "-m", "add big"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid access token"), "{stderr}");
    assert!(stderr.contains("openxet login"), "{stderr}");
    assert!(
        !stderr.contains("wrong"),
        "token leaked into output: {stderr}"
    );

    // Nothing reached git, and fixing the token recovers.
    env.jj(&repo, &["config", "set", "--repo", "xet.token", TOKEN]);
    env.jj(&repo, &["commit", "-m", "add big"]);
    assert_eq!(
        blob_at(&repo, "HEAD", "big.bin"),
        lfs_pointer(&big_contents(11))
    );
}

#[test]
fn credentials_come_from_git_credential_helpers() {
    let env = Env::new();
    let servers = Servers::start(&env);
    let repo = env.init_repo("repo");
    use_openxet(&env, &repo, &servers.lfs_url, None);

    let vars = credential_helper_vars(&env, &servers.lfs_url, TOKEN);

    let contents = big_contents(12);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    env.jj_with(&repo, &vars, &["commit", "-m", "add big"]);
    assert_eq!(blob_at(&repo, "HEAD", "big.bin"), lfs_pointer(&contents));
    assert_eq!(contents.len(), BIG_SIZE);
}

fn credential_helper_vars(
    env: &Env,
    lfs_url: &str,
    password: &str,
) -> [(&'static str, PathBuf); 2] {
    let url = lfs_url.parse::<reqwest::Url>().unwrap();
    let store = env.path("git-credentials");
    fs::write(
        &store,
        format!(
            "http://user:{password}@{}:{}\n",
            url.host_str().unwrap(),
            url.port().unwrap()
        ),
    )
    .unwrap();
    let gitconfig = env.path("gitconfig");
    fs::write(
        &gitconfig,
        format!(
            "[credential]\n\thelper = store --file {}\n",
            store.display()
        ),
    )
    .unwrap();
    [
        ("GIT_CONFIG_GLOBAL", gitconfig),
        ("GIT_CONFIG_NOSYSTEM", PathBuf::from("1")),
    ]
}

#[test]
fn rejected_helper_credentials_are_reported_to_the_helper() {
    let env = Env::new();
    let servers = Servers::start(&env);
    let repo = env.init_repo("repo");
    use_openxet(&env, &repo, &servers.lfs_url, None);
    let vars = credential_helper_vars(&env, &servers.lfs_url, "stale");

    fs::write(repo.join("big.bin"), big_contents(13)).unwrap();
    let output = env.jj_output(&repo, &vars, &["status"]);
    assert!(!output.status.success());
    // `git credential reject` erased the stale entry, so git won't offer it
    // again.
    let store = fs::read_to_string(env.path("git-credentials")).unwrap();
    assert!(!store.contains("stale"), "{store}");
}

#[test]
fn server_must_hand_out_a_network_cas_url() {
    let env = Env::new();
    let servers = Servers::start(&env);
    *servers.mock.cas_url.lock().unwrap() = "memory://".to_owned();
    let repo = env.init_repo("repo");
    use_openxet(&env, &repo, &servers.lfs_url, Some(TOKEN));

    fs::write(repo.join("big.bin"), big_contents(14)).unwrap();
    let output = env.jj_output(&repo, &[], &["commit", "-m", "add big"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unusable casUrl"), "{stderr}");
}

#[test]
fn short_lived_tokens_are_refreshed() {
    let env = Env::new();
    let servers = Servers::start(&env);
    // xet refreshes tokens that expire within 30 seconds.
    servers.mock.token_ttl_secs.store(5, Ordering::Relaxed);
    let repo = env.init_repo("repo");
    use_openxet(&env, &repo, &servers.lfs_url, Some(TOKEN));

    let contents = big_contents(15);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    env.jj(&repo, &["commit", "-m", "add big"]);
    assert!(servers.count("write") >= 2, "{:?}", servers.requests());
    assert_eq!(blob_at(&repo, "HEAD", "big.bin"), lfs_pointer(&contents));
}

#[test]
fn wrong_hashes_from_the_server_are_not_cached() {
    let env = Env::new();
    let servers = Servers::start(&env);
    let repo = env.init_repo("repo");
    use_openxet(&env, &repo, &servers.lfs_url, Some(TOKEN));
    let contents = big_contents(16);
    fs::write(repo.join("big.bin"), &contents).unwrap();
    env.jj(&repo, &["commit", "-m", "add big"]);
    servers.register_uploads(&env.path("cache"));
    let right: Vec<(String, String)> = servers
        .mock
        .xet_hashes
        .lock()
        .unwrap()
        .clone()
        .into_iter()
        .collect();
    // The server maps the oid to the hash of different contents.
    fs::write(repo.join("other.bin"), big_contents(17)).unwrap();
    env.jj(&repo, &["commit", "-m", "add other"]);
    servers.register_uploads(&env.path("cache"));
    {
        let mut hashes = servers.mock.xet_hashes.lock().unwrap();
        let other_hash = hashes
            .iter()
            .find(|(oid, _)| !right.iter().any(|(r, _)| r == *oid))
            .map(|(_, hash)| hash.clone())
            .unwrap();
        hashes.insert(right[0].0.clone(), other_hash);
    }

    git(env.dir.path(), &["clone", "-q", "repo", "clone"]);
    let clone = env.path("clone");
    let machine_b = other_machine(&env);
    env.jj_with(
        &clone,
        &machine_b,
        &[
            "git",
            "init",
            "--colocate",
            "--config",
            "xet.auto-init=false",
        ],
    );
    env.jj_with(
        &clone,
        &machine_b,
        &["config", "set", "--repo", "xet.lfs-url", &servers.lfs_url],
    );
    env.jj_with(
        &clone,
        &machine_b,
        &["config", "set", "--repo", "xet.token", TOKEN],
    );
    let output = env.jj_with(&clone, &machine_b, &["xet", "init"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("leaving the pointer file"));
    assert_eq!(blob_at(&clone, "HEAD~", "big.bin"), lfs_pointer(&contents));

    // Once the server is fixed, a retry succeeds: the bad hash wasn't cached.
    servers
        .mock
        .xet_hashes
        .lock()
        .unwrap()
        .insert(right[0].0.clone(), right[0].1.clone());
    env.jj_with(&clone, &machine_b, &["xet", "init"]);
    assert_eq!(fs::read(clone.join("big.bin")).unwrap(), contents);
}
