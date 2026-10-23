//! Client for an openxet-compatible Git LFS URL.
//!
//! CAS credentials come from `GET {lfs-url}/xet-{read,write}-token`, which
//! returns `{"accessToken", "exp", "casUrl"}` like the Hugging Face Hub token
//! route. Calling it again refreshes the token. The server is authenticated
//! with an access token as the HTTP Basic password, taken from config or from
//! git's credential helpers, just like git-lfs does.

use std::io::Write as _;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::header::ACCEPT;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde::Serialize;
use xet_client::cas_client::auth::AuthError;
use xet_client::cas_client::auth::TokenInfo;
use xet_client::cas_client::auth::TokenRefresher;

use crate::pointer::Pointer;
use crate::pointer::Sha256Hex;

const LFS_MEDIA_TYPE: &str = "application/vnd.git-lfs+json";

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("Invalid LFS URL {url:?}: {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error(
        "No credentials for {url}. Set `xet.token`, or store an access token with `git credential approve`"
    )]
    NoCredentials { url: Url },
    #[error("{url} returned {status}: {message}")]
    Status {
        url: Url,
        status: StatusCode,
        message: String,
    },
    #[error("Request to {url} failed")]
    Http {
        url: Url,
        #[source]
        source: reqwest::Error,
    },
    #[error("Running `git credential` failed")]
    CredentialHelper(#[source] std::io::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

impl Access {
    fn route(self) -> &'static str {
        match self {
            Self::Read => "xet-read-token",
            Self::Write => "xet-write-token",
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CasToken {
    pub access_token: String,
    /// Unix seconds.
    pub exp: u64,
    pub cas_url: String,
}

impl std::fmt::Debug for CasToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CasToken")
            .field("exp", &self.exp)
            .field("cas_url", &self.cas_url)
            .finish_non_exhaustive()
    }
}

/// openxet error bodies (`openxet.error.v1`).
#[derive(Deserialize)]
struct ErrorBody {
    message: String,
    hint: Option<String>,
}

#[derive(Clone)]
struct Credential {
    username: Option<String>,
    password: String,
    from_helper: bool,
}

pub struct LfsRemote {
    url: Url,
    configured_token: Option<String>,
    http: reqwest::Client,
    credential: Mutex<Option<Credential>>,
    /// Whether the credential helper was told the credential works. Once per
    /// process is enough; helpers like `store` rewrite a file every time.
    approved: AtomicBool,
}

impl std::fmt::Debug for LfsRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print credentials.
        f.debug_struct("LfsRemote")
            .field("url", &self.url.as_str())
            .finish_non_exhaustive()
    }
}

impl LfsRemote {
    pub fn new(url: &str, configured_token: Option<String>) -> Result<Self, RemoteError> {
        // Error messages must not echo credentials embedded in the URL.
        let redacted = match Url::parse(url) {
            Ok(mut parsed) => {
                let _ = parsed.set_username("");
                let _ = parsed.set_password(None);
                parsed.to_string()
            }
            Err(_) => "<unparseable>".to_owned(),
        };
        let invalid = |reason: &str| RemoteError::InvalidUrl {
            url: redacted.clone(),
            reason: reason.to_owned(),
        };
        let mut url = Url::parse(url).map_err(|err| invalid(&err.to_string()))?;
        if !matches!(url.scheme(), "https" | "http") {
            return Err(invalid("must be an http(s) URL"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(invalid(
                "must not contain credentials; use `xet.token` instead",
            ));
        }
        // Make relative joins append to the path instead of replacing the
        // last segment.
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("jj-xet/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| invalid(&err.to_string()))?;
        Ok(Self {
            url,
            configured_token,
            http,
            credential: Mutex::new(None),
            approved: AtomicBool::new(false),
        })
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    fn endpoint(&self, route: &str) -> Url {
        self.url.join(route).expect("route is a valid relative URL")
    }

    async fn credential(&self) -> Result<Credential, RemoteError> {
        if let Some(credential) = self.credential.lock().unwrap().clone() {
            return Ok(credential);
        }
        let credential = match &self.configured_token {
            Some(token) => Credential {
                username: None,
                password: token.clone(),
                from_helper: false,
            },
            None => {
                let url = self.url.clone();
                tokio::task::spawn_blocking(move || git_credential_fill(&url))
                    .await
                    .expect("credential helper task panicked")?
                    .ok_or_else(|| RemoteError::NoCredentials {
                        url: self.url.clone(),
                    })?
            }
        };
        *self.credential.lock().unwrap() = Some(credential.clone());
        Ok(credential)
    }

    /// Sends an authenticated request, telling the credential helper whether
    /// its credential worked, like git does.
    async fn send(
        &self,
        url: Url,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, RemoteError> {
        let credential = self.credential().await?;
        let username = credential.username.as_deref().unwrap_or("jj-xet");
        let response = request
            .basic_auth(username, Some(&credential.password))
            .send()
            .await
            .map_err(|source| RemoteError::Http {
                url: url.clone(),
                source,
            })?;
        let status = response.status();
        let action = if status.is_success() {
            (!self.approved.swap(true, Ordering::Relaxed)).then_some("approve")
        } else if status == StatusCode::UNAUTHORIZED {
            self.approved.store(false, Ordering::Relaxed);
            Some("reject")
        } else {
            None
        };
        if credential.from_helper
            && let Some(action) = action
        {
            let helper_url = self.url.clone();
            // Best effort: a failing helper must not fail the request.
            let _ = tokio::task::spawn_blocking(move || {
                git_credential(action, &helper_url, Some(&credential))
            })
            .await;
        }
        if status == StatusCode::UNAUTHORIZED {
            self.credential.lock().unwrap().take();
        }
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        let message = match serde_json::from_str::<ErrorBody>(&body) {
            Ok(ErrorBody {
                message,
                hint: Some(hint),
            }) => format!("{message} (hint: {hint})"),
            Ok(ErrorBody {
                message,
                hint: None,
            }) => message,
            Err(_) => body.trim().chars().take(200).collect(),
        };
        // Server text goes to the terminal; drop escape sequences.
        let message = message.chars().filter(|c| !c.is_control()).collect();
        Err(RemoteError::Status {
            url,
            status,
            message,
        })
    }

    pub async fn cas_token(&self, access: Access) -> Result<CasToken, RemoteError> {
        let url = self.endpoint(access.route());
        let request = self
            .http
            .get(url.clone())
            .header(ACCEPT, "application/json");
        let response = self.send(url.clone(), request).await?;
        let token: CasToken = response.json().await.map_err(|source| RemoteError::Http {
            url: url.clone(),
            source,
        })?;
        // xet picks its storage backend from the URL scheme, so a server
        // answering `local://` or `memory://` would make uploads silently
        // land on this machine or nowhere.
        let scheme_ok = Url::parse(&token.cas_url).is_ok_and(|cas_url| {
            cas_url.scheme() == "https"
                || (cas_url.scheme() == "http" && self.url.scheme() == "http")
        });
        if !scheme_ok {
            return Err(RemoteError::Status {
                url,
                status: StatusCode::OK,
                message: format!(
                    "unusable casUrl {:?}; expected an {} URL",
                    token.cas_url,
                    self.url.scheme()
                ),
            });
        }
        Ok(token)
    }

    /// Asks the server for the Xet hash of a pointer through the LFS batch
    /// API. Returns `None` if the server doesn't know the object.
    pub async fn lookup_xet_hash(&self, pointer: &Pointer) -> Result<Option<String>, RemoteError> {
        let url = self.endpoint("objects/batch");
        let body = BatchRequest {
            operation: "download",
            transfers: &["xet", "basic"],
            objects: &[BatchObject {
                oid: pointer.oid.as_str(),
                size: pointer.size,
            }],
            hash_algo: "sha256",
        };
        let request = self
            .http
            .post(url.clone())
            .header(ACCEPT, LFS_MEDIA_TYPE)
            .header(CONTENT_TYPE, LFS_MEDIA_TYPE)
            .json(&body);
        let response: BatchResponse = self
            .send(url.clone(), request)
            .await?
            .json()
            .await
            .map_err(|source| RemoteError::Http { url, source })?;
        let hash = response
            .objects
            .into_iter()
            .find(|object| object.oid == pointer.oid.as_str())
            .and_then(|object| object.actions?.download?.header?.xet_hash)
            .and_then(|hash| Sha256Hex::parse(&hash))
            .map(|hash| hash.as_str().to_owned());
        Ok(hash)
    }
}

#[derive(Serialize)]
struct BatchRequest<'a> {
    operation: &'a str,
    transfers: &'a [&'a str],
    objects: &'a [BatchObject<'a>],
    hash_algo: &'a str,
}

#[derive(Serialize)]
struct BatchObject<'a> {
    oid: &'a str,
    size: u64,
}

#[derive(Deserialize)]
struct BatchResponse {
    objects: Vec<BatchResponseObject>,
}

#[derive(Deserialize)]
struct BatchResponseObject {
    oid: String,
    actions: Option<BatchActions>,
}

#[derive(Deserialize)]
struct BatchActions {
    download: Option<BatchAction>,
}

#[derive(Deserialize)]
struct BatchAction {
    header: Option<BatchHeaders>,
}

#[derive(Deserialize)]
struct BatchHeaders {
    // Exact casing is part of openxet's contract (ADR 0005), matching
    // git-xet's constants.
    #[serde(rename = "X-Xet-Hash")]
    xet_hash: Option<String>,
}

/// Refreshes CAS tokens by calling the token route again. xet's refresher
/// interface only carries the token, so the `casUrl` of the first response
/// stays in use for the session.
#[derive(Debug)]
pub struct Refresher {
    pub remote: Arc<LfsRemote>,
    pub access: Access,
}

#[async_trait]
impl TokenRefresher for Refresher {
    async fn refresh(&self) -> Result<TokenInfo, AuthError> {
        let token = self
            .remote
            .cas_token(self.access)
            .await
            .map_err(AuthError::token_refresh_failure)?;
        Ok((token.access_token, token.exp))
    }
}

fn credential_input(url: &Url, credential: Option<&Credential>) -> String {
    let mut input = format!(
        "protocol={}\nhost={}",
        url.scheme(),
        url.host_str().unwrap_or("")
    );
    if let Some(port) = url.port() {
        input.push_str(&format!(":{port}"));
    }
    input.push_str(&format!("\npath={}\n", url.path().trim_matches('/')));
    if let Some(credential) = credential {
        if let Some(username) = &credential.username {
            input.push_str(&format!("username={username}\n"));
        }
        input.push_str(&format!("password={}\n", credential.password));
    }
    input.push('\n');
    input
}

fn git_credential(
    action: &str,
    url: &Url,
    credential: Option<&Credential>,
) -> Result<String, RemoteError> {
    let mut child = Command::new("git")
        .args(["credential", action])
        // jj may be running without a terminal; only consult helpers.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(RemoteError::CredentialHelper)?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(credential_input(url, credential).as_bytes())
        .map_err(RemoteError::CredentialHelper)?;
    let output = child
        .wait_with_output()
        .map_err(RemoteError::CredentialHelper)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git_credential_fill(url: &Url) -> Result<Option<Credential>, RemoteError> {
    let output = git_credential("fill", url, None)?;
    let mut username = None;
    let mut password = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("username=") {
            username = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("password=") {
            password = Some(value.to_owned());
        }
    }
    Ok(password.map(|password| Credential {
        username,
        password,
        from_helper: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_routes_onto_the_lfs_url() {
        let remote = LfsRemote::new("https://host/ns/repo.git/info/lfs", None).unwrap();
        assert_eq!(
            remote.endpoint("xet-write-token").as_str(),
            "https://host/ns/repo.git/info/lfs/xet-write-token"
        );
        assert_eq!(
            remote.endpoint("objects/batch").as_str(),
            "https://host/ns/repo.git/info/lfs/objects/batch"
        );
    }

    #[test]
    fn rejects_bad_urls() {
        for url in ["ftp://host/lfs", "https://user:pw@host/lfs", "not a url"] {
            assert!(LfsRemote::new(url, None).is_err(), "{url}");
        }
    }

    #[test]
    fn formats_credential_helper_input() {
        let url = Url::parse("https://host:8443/ns/repo.git/info/lfs/").unwrap();
        assert_eq!(
            credential_input(&url, None),
            "protocol=https\nhost=host:8443\npath=ns/repo.git/info/lfs\n\n"
        );
    }

    #[test]
    fn invalid_url_errors_redact_credentials() {
        let err = LfsRemote::new("https://user:hunter2@host/lfs", None).unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }

    #[test]
    fn debug_hides_token() {
        let remote = LfsRemote::new("https://host/lfs", Some("secret".to_owned())).unwrap();
        assert!(!format!("{remote:?}").contains("secret"));
    }
}
