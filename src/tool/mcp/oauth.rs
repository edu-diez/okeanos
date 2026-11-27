//! OAuth for Streamable HTTP MCP servers (M35).
//!
//! `okeanos mcp login <server>` runs the authorization-code + PKCE flow with a
//! one-shot loopback redirect listener and stores the result in a per-server
//! file (mode `0600` on Unix) outside the config. At connect time the stored
//! credentials are loaded and rmcp refreshes tokens, saving them back here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    StoredCredentials,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::client::{McpError, McpServerConfig};

/// How long `mcp login` waits for the browser redirect.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Upper bound on a redirect request head we are willing to read.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

fn auth_err(e: AuthError) -> McpError {
    McpError::AuthFailed(e.to_string())
}

/// Credential file location for a server, keyed by name and URL so that
/// repointing a server at another URL never reuses its tokens.
pub fn store_path_in(dir: &Path, cfg: &McpServerConfig) -> PathBuf {
    let name: String = cfg
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let digest = Sha256::digest(cfg.url.as_deref().unwrap_or_default().as_bytes());
    let hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}-{hash}.json"))
}

/// Default credential file for a server (`<data_dir>/okeanos/mcp-auth/`).
pub fn store_path(cfg: &McpServerConfig) -> Option<PathBuf> {
    Some(store_path_in(
        &dirs::data_dir()?.join("okeanos").join("mcp-auth"),
        cfg,
    ))
}

/// File-backed rmcp credential store. Contents are secrets: never log them.
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

fn store_err(e: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(e.to_string())
}

/// Atomically write `bytes` to `path`, readable only by the owner on Unix.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(store_err),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let json = serde_json::to_vec(&credentials).map_err(store_err)?;
        write_private(&self.path, &json).map_err(store_err)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(store_err(e)),
        }
    }
}

/// Build an authorization manager from stored credentials, or `None` when the
/// server has no usable login (the caller then connects unauthenticated and
/// surfaces a "run `okeanos mcp login`" hint on a 401).
pub async fn authorized_manager(
    cfg: &McpServerConfig,
) -> Result<Option<AuthorizationManager>, McpError> {
    let (Some(url), Some(path)) = (cfg.url.as_deref(), store_path(cfg)) else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let mut manager = AuthorizationManager::new(url).await.map_err(auth_err)?;
    manager.set_credential_store(FileCredentialStore::new(path));
    if manager.initialize_from_store().await.map_err(auth_err)? {
        Ok(Some(manager))
    } else {
        Ok(None)
    }
}

/// Remove a server's stored credentials. Returns whether anything was removed.
pub fn logout(cfg: &McpServerConfig) -> Result<bool, McpError> {
    let Some(path) = store_path(cfg) else {
        return Ok(false);
    };
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// What a request received on the redirect listener means.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CallbackRequest {
    /// Not the redirect path (favicon, probes): ignore.
    Other,
    /// The authorization server reported an error.
    Denied(String),
    /// The request target (`/callback?code=...&state=...`) to complete login.
    Target(String),
}

pub(crate) fn parse_callback_request(head: &str) -> CallbackRequest {
    let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return CallbackRequest::Other;
    };
    let Some(query) = target.strip_prefix("/callback?") else {
        return CallbackRequest::Other;
    };
    let error = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("error="))
        .map(|v| {
            let desc = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("error_description="));
            match desc {
                Some(d) => format!("{v}: {d}"),
                None => v.to_string(),
            }
        });
    match error {
        Some(e) => CallbackRequest::Denied(e),
        None => CallbackRequest::Target(target.to_string()),
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(msg.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn read_head(stream: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < MAX_REQUEST_BYTES && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Run the interactive login for an HTTP server. `announce` receives the
/// authorization URL (to print and/or open in a browser).
pub async fn login(cfg: &McpServerConfig, announce: impl FnOnce(&str)) -> Result<(), McpError> {
    let url = cfg.url.as_deref().ok_or_else(|| {
        McpError::AuthFailed(format!(
            "server '{}' is not a Streamable HTTP server (no `url`)",
            cfg.name
        ))
    })?;
    let path = store_path(cfg)
        .ok_or_else(|| McpError::AuthFailed("no user data directory available".to_string()))?;

    // rmcp's reqwest has no bundled TLS provider.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );

    let mut manager = AuthorizationManager::new(url).await.map_err(auth_err)?;
    manager.set_credential_store(FileCredentialStore::new(path));
    let resolution = manager.resolve_metadata().await.map_err(auth_err)?;
    manager.set_metadata(resolution.metadata);
    let session = AuthorizationSession::new(
        manager,
        AuthorizationRequest::new(redirect.clone()).with_client_name("okeanos"),
    )
    .await
    .map_err(|(_, e)| auth_err(e))?;

    announce(session.get_authorization_url());

    let wait = async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let head = read_head(&mut stream).await;
            match parse_callback_request(&head) {
                CallbackRequest::Other => {
                    respond(&mut stream, "404 Not Found", "Not found").await;
                }
                CallbackRequest::Denied(e) => {
                    respond(&mut stream, "400 Bad Request", "Authorization failed.").await;
                    return Err(McpError::AuthFailed(format!("authorization denied: {e}")));
                }
                CallbackRequest::Target(target) => {
                    let full = format!("http://127.0.0.1:{}{target}", redirect_port(&redirect));
                    return match session.handle_callback_url(&full).await {
                        Ok(_) => {
                            respond(
                                &mut stream,
                                "200 OK",
                                "Logged in. You can close this tab and return to okeanos.",
                            )
                            .await;
                            Ok(())
                        }
                        Err(e) => {
                            respond(&mut stream, "400 Bad Request", "Authorization failed.").await;
                            Err(auth_err(e))
                        }
                    };
                }
            }
        }
    };
    tokio::time::timeout(LOGIN_TIMEOUT, wait)
        .await
        .map_err(|_| McpError::AuthFailed("timed out waiting for the browser redirect".into()))?
}

fn redirect_port(redirect: &str) -> &str {
    redirect
        .rsplit_once(':')
        .and_then(|(_, rest)| rest.split('/').next())
        .unwrap_or_default()
}

/// Best-effort: open `url` in the user's browser via a platform helper.
pub fn open_browser(url: &str) {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Hint appended to connection errors of HTTP servers that look like auth failures.
pub fn auth_hint(server: &str, error: &str) -> Option<String> {
    let e = error.to_ascii_lowercase();
    (e.contains("auth") || e.contains("401") || e.contains("unauthorized"))
        .then(|| format!("run `okeanos mcp login {server}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str, url: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            url: Some(url.into()),
            ..Default::default()
        }
    }

    #[test]
    fn store_path_is_keyed_by_name_and_url() {
        let dir = Path::new("/d");
        let a = store_path_in(dir, &cfg("my srv/../x", "https://a.example/mcp"));
        let b = store_path_in(dir, &cfg("my srv/../x", "https://b.example/mcp"));
        assert_ne!(a, b);
        let file = a.file_name().unwrap().to_str().unwrap();
        assert!(file.starts_with("my_srv____x-") && file.ends_with(".json"));
        assert_eq!(a.parent().unwrap(), dir);
    }

    #[tokio::test]
    async fn credential_store_round_trip_and_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("s.json");
        let store = FileCredentialStore::new(path.clone());
        assert!(store.load().await.unwrap().is_none());

        let creds = StoredCredentials::new("client-1".into(), None, vec!["read".into()], Some(7));
        store.save(creds).await.unwrap();
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "client-1");
        assert_eq!(loaded.granted_scopes, vec!["read".to_string()]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(!path.with_extension("json.tmp").exists());

        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        store.clear().await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_store_is_an_error_not_a_silent_logout() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.json");
        std::fs::write(&path, b"not json").unwrap();
        let err = FileCredentialStore::new(path).load().await.unwrap_err();
        assert!(matches!(err, AuthError::CredentialStoreError(_)));
    }

    #[test]
    fn parses_callback_requests() {
        assert_eq!(
            parse_callback_request("GET /favicon.ico HTTP/1.1\r\n\r\n"),
            CallbackRequest::Other
        );
        assert_eq!(
            parse_callback_request("POST /callback?code=a HTTP/1.1\r\n\r\n"),
            CallbackRequest::Other
        );
        assert_eq!(
            parse_callback_request("GET /callback?code=abc&state=xyz HTTP/1.1\r\nHost: x\r\n\r\n"),
            CallbackRequest::Target("/callback?code=abc&state=xyz".into())
        );
        assert_eq!(
            parse_callback_request(
                "GET /callback?error=access_denied&error_description=no HTTP/1.1\r\n\r\n"
            ),
            CallbackRequest::Denied("access_denied: no".into())
        );
    }

    #[test]
    fn redirect_port_extraction() {
        assert_eq!(redirect_port("http://127.0.0.1:5123/callback"), "5123");
    }

    #[test]
    fn auth_hint_only_for_auth_like_errors() {
        assert!(auth_hint("s", "Auth required, when send initialize request").is_some());
        assert!(auth_hint("s", "HTTP status 401").is_some());
        assert!(auth_hint("s", "connection refused").is_none());
    }
}
