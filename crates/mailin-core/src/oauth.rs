//! OAuth2 authorization-code flow with PKCE for IMAP/SMTP (XOAUTH2).
//!
//! The flow is intentionally dependency-light and platform-neutral:
//!
//! 1. [`PendingAuthorization::begin`] opens a loopback listener and builds the
//!    authorization URL.
//! 2. The front-end opens that URL in the system browser.
//! 3. [`PendingAuthorization::wait`] blocks until the browser redirects back,
//!    validates `state`, and exchanges the authorization code for tokens.
//!
//! Tokens are returned as [`OAuth2Tokens`] so the caller decides where to store
//! them (Mailin keeps them in the account row; the keyring holds only
//! passwords).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::account::{OAuth2Tokens, OAuthProvider};
use crate::error::{Error, Result};

/// Endpoint configuration for one provider.
#[derive(Debug, Clone, Copy)]
pub struct ProviderEndpoints {
    pub auth_url: &'static str,
    pub token_url: &'static str,
    pub scopes: &'static str,
    /// Extra query parameters for the authorization request, for example
    /// `access_type=offline` so Google issues a refresh token.
    pub extra_auth_params: &'static [(&'static str, &'static str)],
    /// Loopback host. Microsoft only allows `localhost` for native clients.
    pub loopback_host: &'static str,
}

impl OAuthProvider {
    /// Endpoints and scopes for the provider.
    pub const fn endpoints(self) -> ProviderEndpoints {
        match self {
            Self::Google => ProviderEndpoints {
                auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
                token_url: "https://oauth2.googleapis.com/token",
                scopes: "https://mail.google.com/ openid email profile",
                extra_auth_params: &[("access_type", "offline"), ("prompt", "consent")],
                loopback_host: "127.0.0.1",
            },
            Self::Microsoft => ProviderEndpoints {
                auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
                token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
                scopes: "https://outlook.office.com/IMAP.AccessAsUser.All \
                         https://outlook.office.com/SMTP.Send offline_access openid email profile",
                extra_auth_params: &[],
                loopback_host: "localhost",
            },
            Self::Yahoo => ProviderEndpoints {
                auth_url: "https://api.login.yahoo.com/oauth2/request_auth",
                token_url: "https://api.login.yahoo.com/oauth2/get_token",
                scopes: "mail-w openid email profile",
                extra_auth_params: &[],
                loopback_host: "127.0.0.1",
            },
            Self::Fastmail => ProviderEndpoints {
                auth_url: "https://api.fastmail.com/oauth/authorize",
                token_url: "https://api.fastmail.com/oauth/token",
                scopes: "urn:ietf:params:oauth:scope:mail offline_access",
                extra_auth_params: &[],
                loopback_host: "127.0.0.1",
            },
            Self::Custom => ProviderEndpoints {
                auth_url: "",
                token_url: "",
                scopes: "",
                extra_auth_params: &[],
                loopback_host: "127.0.0.1",
            },
        }
    }

    /// Human readable provider name.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Google => "Google / Gmail",
            Self::Microsoft => "Microsoft 365 / Outlook",
            Self::Yahoo => "Yahoo Mail",
            Self::Fastmail => "Fastmail",
            Self::Custom => "Custom provider",
        }
    }

    /// Environment variable that supplies the application's public client ID.
    pub const fn client_id_env(self) -> &'static str {
        match self {
            Self::Google => "MAILIN_GOOGLE_CLIENT_ID",
            Self::Microsoft => "MAILIN_MICROSOFT_CLIENT_ID",
            Self::Yahoo => "MAILIN_YAHOO_CLIENT_ID",
            Self::Fastmail => "MAILIN_FASTMAIL_CLIENT_ID",
            Self::Custom => "MAILIN_OAUTH_CLIENT_ID",
        }
    }

    /// Client ID from the environment, or an empty string when it is not set.
    ///
    /// OAuth2 client IDs are public identifiers but each application registers
    /// its own; shipping one in source is neither possible nor correct. The
    /// account dialog lets the user paste one, and this helper provides the
    /// default for deployments that configure it.
    pub fn default_client_id(self) -> String {
        std::env::var(self.client_id_env()).unwrap_or_default()
    }

    /// Resolve the endpoints, honouring `MAILIN_OAUTH_*` overrides for custom
    /// providers.
    pub fn resolved_endpoints(self) -> Result<ResolvedEndpoints> {
        let base = self.endpoints();
        let auth_url = if base.auth_url.is_empty() {
            std::env::var("MAILIN_OAUTH_AUTH_URL").unwrap_or_default()
        } else {
            base.auth_url.to_owned()
        };
        let token_url = if base.token_url.is_empty() {
            std::env::var("MAILIN_OAUTH_TOKEN_URL").unwrap_or_default()
        } else {
            base.token_url.to_owned()
        };
        let scopes = if base.scopes.is_empty() {
            std::env::var("MAILIN_OAUTH_SCOPES").unwrap_or_default()
        } else {
            base.scopes.to_owned()
        };
        if auth_url.is_empty() || token_url.is_empty() {
            return Err(Error::OAuth(
                "OAuth2 endpoints are not configured; set MAILIN_OAUTH_AUTH_URL and \
                 MAILIN_OAUTH_TOKEN_URL"
                    .into(),
            ));
        }
        Ok(ResolvedEndpoints {
            auth_url,
            token_url,
            scopes,
            extra_auth_params: base.extra_auth_params,
            loopback_host: base.loopback_host,
        })
    }
}

/// Provider endpoints after applying environment overrides.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoints {
    pub auth_url: String,
    pub token_url: String,
    pub scopes: String,
    pub extra_auth_params: &'static [(&'static str, &'static str)],
    pub loopback_host: &'static str,
}

/// An authorization attempt that is waiting for the browser redirect.
#[derive(Debug)]
pub struct PendingAuthorization {
    listener: TcpListener,
    state: String,
    verifier: String,
    redirect_uri: String,
    auth_url: String,
    token_url: String,
    provider: OAuthProvider,
    client_id: String,
}

impl PendingAuthorization {
    /// Bind the loopback listener and build the authorization URL.
    ///
    /// The caller must open [`auth_url`](Self::auth_url) in a browser and then
    /// call [`wait`](Self::wait) from a blocking thread.
    pub fn begin(
        provider: OAuthProvider,
        client_id: &str,
        login_hint: Option<&str>,
    ) -> Result<Self> {
        if client_id.trim().is_empty() {
            return Err(Error::OAuth(
                "an OAuth2 client ID is required: enter one in the account dialog \
                 or set the provider's MAILIN_*_CLIENT_ID environment variable"
                    .into(),
            ));
        }

        let endpoints = provider.resolved_endpoints()?;
        let listener = TcpListener::bind((endpoints.loopback_host, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://{}:{port}", endpoints.loopback_host);

        let verifier = random_token();
        let state = random_token();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}\
             &code_challenge={}&code_challenge_method=S256",
            endpoints.auth_url,
            percent_encode(client_id),
            percent_encode(&redirect_uri),
            percent_encode(&endpoints.scopes),
            percent_encode(&state),
            percent_encode(&challenge),
        );
        for (key, value) in endpoints.extra_auth_params {
            url.push('&');
            url.push_str(key);
            url.push('=');
            url.push_str(&percent_encode(value));
        }
        if let Some(hint) = login_hint.filter(|hint| !hint.trim().is_empty()) {
            url.push_str("&login_hint=");
            url.push_str(&percent_encode(hint));
        }

        Ok(Self {
            listener,
            state,
            verifier,
            redirect_uri,
            auth_url: url,
            token_url: endpoints.token_url,
            provider,
            client_id: client_id.trim().to_owned(),
        })
    }

    /// URL to open in the system browser.
    pub fn auth_url(&self) -> &str {
        &self.auth_url
    }

    /// Redirect URI registered for the loopback listener.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait for the redirect, then exchange the authorization code.
    ///
    /// This blocks the calling thread and should run off the UI thread.
    pub fn wait(self, timeout: Duration) -> Result<OAuth2Tokens> {
        let code = self.wait_for_code(timeout)?;
        exchange_code(
            self.provider,
            &self.client_id,
            &self.token_url,
            &code,
            &self.redirect_uri,
            &self.verifier,
        )
    }

    fn wait_for_code(&self, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let mut stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(Error::OAuth(
                            "timed out waiting for the browser to complete sign-in".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(Error::Io(error)),
            }
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;

        let request = read_request(&mut stream)?;
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default();
        let query = target.split_once('?').map_or("", |(_, query)| query);
        let params = parse_query(query);

        if let Some(error) = params.get("error") {
            let description = params
                .get("error_description")
                .map(String::as_str)
                .unwrap_or(error);
            respond(&mut stream, false, "Sign-in failed", description);
            return Err(Error::OAuth(format!("authorization failed: {description}")));
        }

        let returned_state = params.get("state").map(String::as_str).unwrap_or_default();
        if returned_state != self.state {
            respond(
                &mut stream,
                false,
                "Sign-in failed",
                "The authorization response did not match this request.",
            );
            return Err(Error::OAuth(
                "OAuth2 state mismatch; the response may have been forged".into(),
            ));
        }

        let Some(code) = params.get("code") else {
            respond(
                &mut stream,
                false,
                "Sign-in failed",
                "The provider did not return an authorization code.",
            );
            return Err(Error::OAuth(
                "the authorization response did not include a code".into(),
            ));
        };

        respond(
            &mut stream,
            true,
            "Mailin is signed in",
            "You can close this browser tab and return to Mailin.",
        );
        Ok(code.clone())
    }
}

/// Exchange an authorization code for tokens.
pub fn exchange_code(
    provider: OAuthProvider,
    client_id: &str,
    token_url: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<OAuth2Tokens> {
    let response: TokenResponse = ureq::post(token_url)
        .send_form([
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ])
        .map_err(|error| Error::OAuth(format!("token request failed: {error}")))?
        .body_mut()
        .read_json()
        .map_err(|error| Error::OAuth(format!("could not read the token response: {error}")))?;

    response.into_tokens(provider, client_id, "")
}

/// Use a refresh token to obtain a fresh access token.
pub fn refresh(
    provider: OAuthProvider,
    client_id: &str,
    refresh_token: &str,
) -> Result<OAuth2Tokens> {
    if refresh_token.is_empty() {
        return Err(Error::OAuth("the account has no refresh token".into()));
    }
    let endpoints = provider.resolved_endpoints()?;

    let response: TokenResponse = ureq::post(&endpoints.token_url)
        .send_form([
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .map_err(|error| Error::OAuth(format!("token refresh failed: {error}")))?
        .body_mut()
        .read_json()
        .map_err(|error| Error::OAuth(format!("could not read the refresh response: {error}")))?;

    response.into_tokens(provider, client_id, refresh_token)
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default = "default_expires_in")]
    expires_in: i64,
}

fn default_expires_in() -> i64 {
    3600
}

impl TokenResponse {
    fn into_tokens(
        self,
        provider: OAuthProvider,
        client_id: &str,
        existing_refresh_token: &str,
    ) -> Result<OAuth2Tokens> {
        if self.access_token.is_empty() {
            return Err(Error::OAuth(
                "the provider returned an empty access token".into(),
            ));
        }
        let refresh_token = if self.refresh_token.is_empty() {
            existing_refresh_token.to_owned()
        } else {
            self.refresh_token
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or_default();
        Ok(OAuth2Tokens {
            access_token: self.access_token,
            refresh_token,
            expires_at: now + self.expires_in.max(60),
            provider,
            client_id: client_id.to_owned(),
        })
    }
}

fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") || buffer.len() > 16 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn respond(stream: &mut TcpStream, success: bool, title: &str, message: &str) {
    let colour = if success { "#1a7f37" } else { "#b42318" };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <style>body{{font-family:system-ui,sans-serif;margin:3rem;line-height:1.5}}\
         h1{{color:{colour};font-size:1.4rem}}</style>\
         <h1>{title}</h1><p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn random_token() -> String {
    // Two UUIDv4 values give 244 bits of entropy and satisfy PKCE's 43..=128
    // character requirement after hex encoding.
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = &value[index + 1..index + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        decoded.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        decoded.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_round_trips() {
        let value = "https://example.com/a b?x=1&y=z";
        assert_eq!(percent_decode(&percent_encode(value)), value);
    }

    #[test]
    fn query_parsing_decodes_values() {
        let params = parse_query("code=abc%2F123&state=xyz&error_description=a+b");
        assert_eq!(params.get("code").map(String::as_str), Some("abc/123"));
        assert_eq!(
            params.get("error_description").map(String::as_str),
            Some("a b")
        );
    }

    #[test]
    fn token_response_keeps_the_existing_refresh_token() {
        let response = TokenResponse {
            access_token: "new-access".into(),
            refresh_token: String::new(),
            expires_in: 3600,
        };
        let tokens = response
            .into_tokens(OAuthProvider::Google, "client", "old-refresh")
            .expect("tokens");
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token, "old-refresh");
        assert!(tokens.expires_at > 0);
    }

    #[test]
    fn begin_requires_a_client_id() {
        let error = PendingAuthorization::begin(OAuthProvider::Google, "  ", None)
            .expect_err("client id is required");
        assert!(error.to_string().contains("client ID"));
    }

    #[test]
    fn begin_builds_pkce_authorization_url() {
        let pending = PendingAuthorization::begin(
            OAuthProvider::Google,
            "client-id",
            Some("ada@example.com"),
        )
        .expect("pending authorization");
        let url = pending.auth_url();
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state="));
        assert!(url.contains("login_hint=ada%40example.com"));
        assert!(pending.redirect_uri().starts_with("http://127.0.0.1:"));
    }

    #[test]
    fn microsoft_uses_the_localhost_loopback_host() {
        let pending = PendingAuthorization::begin(OAuthProvider::Microsoft, "client-id", None)
            .expect("pending authorization");
        assert!(pending.redirect_uri().starts_with("http://localhost:"));
    }
}
