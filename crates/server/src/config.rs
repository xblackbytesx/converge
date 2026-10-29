use anyhow::{Context, Result, bail};
use base64::Engine as _;

pub struct Config {
    pub database_url: String,
    pub bind: String,
    /// Master key for connection credentials at rest (32 bytes). Required
    /// from the first start, so it exists before the first credential does.
    pub secret_key: [u8; 32],
    /// `Secure` on the web session cookie. On by default; turn off only for a
    /// plain-http LAN setup, or browsers will not send the cookie back.
    pub cookie_secure: bool,
    /// This server as people reach it (`https://converge.example.com`), for
    /// invite links. Without it the request's own host is used.
    pub public_url: Option<String>,
    /// `CONVERGE_BEHIND_PROXY`: the right-most `X-Forwarded-For` entry is
    /// the client (what Traefik saw). Declared, never guessed: off, the
    /// header is ignored, because anyone can send one.
    pub behind_proxy: bool,
    /// `CONVERGE_METRICS_BIND`: the internal port for `/metrics` and
    /// `/healthz`. None: `off`.
    pub metrics_bind: Option<String>,
    /// `CONVERGE_METRICS_TOKEN`: bearer token for `/metrics` when set.
    pub metrics_token: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: var("DATABASE_URL").context("DATABASE_URL is required")?,
            bind: var("CONVERGE_BIND").unwrap_or_else(|| "0.0.0.0:8080".into()),
            secret_key: secret_key(var("CONVERGE_SECRET_KEY").as_deref())?,
            cookie_secure: !is_off(var("CONVERGE_COOKIE_SECURE").as_deref()),
            public_url: public_url(var("CONVERGE_PUBLIC_URL").as_deref())?,
            behind_proxy: is_on(var("CONVERGE_BEHIND_PROXY").as_deref()),
            metrics_bind: metrics_bind(var("CONVERGE_METRICS_BIND")),
            metrics_token: var("CONVERGE_METRICS_TOKEN"),
        })
    }
}

fn is_off(v: Option<&str>) -> bool {
    matches!(v, Some("false" | "0" | "no"))
}

fn is_on(v: Option<&str>) -> bool {
    matches!(v, Some("true" | "1" | "yes"))
}

/// Declared, never generated on the fly: a key made at startup and lost with
/// the container would silently orphan every stored credential.
pub fn secret_key(v: Option<&str>) -> Result<[u8; 32]> {
    let Some(v) = v else {
        bail!("CONVERGE_SECRET_KEY is required: 32 random bytes, base64 (`head -c 32 /dev/urandom | base64`)")
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(v)
        .context("CONVERGE_SECRET_KEY must be base64 (`head -c 32 /dev/urandom | base64`)")?;
    bytes.try_into().map_err(|b: Vec<u8>| anyhow::anyhow!("CONVERGE_SECRET_KEY must be 32 bytes, got {}", b.len()))
}

pub fn public_url(v: Option<&str>) -> Result<Option<String>> {
    let Some(v) = v else { return Ok(None) };
    if !(v.starts_with("https://") || v.starts_with("http://")) {
        bail!("CONVERGE_PUBLIC_URL must start with https:// (or http:// on a LAN)");
    }
    Ok(Some(v.trim_end_matches('/').to_string()))
}

fn metrics_bind(v: Option<String>) -> Option<String> {
    match v.as_deref() {
        None => Some("0.0.0.0:9090".into()),
        Some("off") => None,
        Some(_) => v,
    }
}

/// Trimmed; empty means unset.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_must_be_32_base64_bytes() {
        assert!(secret_key(None).is_err());
        assert!(secret_key(Some("not base64!")).is_err());
        assert!(secret_key(Some("c2hvcnQ=")).is_err());
        assert_eq!(
            secret_key(Some("ZGV2LW9ubHktZGV2LW9ubHktZGV2LW9ubHktZGV2LW8=")).unwrap(),
            *b"dev-only-dev-only-dev-only-dev-o"
        );
    }

    #[test]
    fn public_url_is_a_base_url() {
        assert_eq!(public_url(None).unwrap(), None);
        assert_eq!(public_url(Some("https://c.example.com/")).unwrap().as_deref(), Some("https://c.example.com"));
        assert!(public_url(Some("c.example.com")).is_err());
    }

    #[test]
    fn switches_default_safe() {
        assert!(!is_off(None), "cookie Secure stays on unless turned off");
        assert!(!is_on(None), "X-Forwarded-For is ignored unless declared");
        assert!(is_on(Some("true")) && is_off(Some("false")));
    }

    #[test]
    fn metrics_port_is_on_unless_off() {
        assert_eq!(metrics_bind(None).as_deref(), Some("0.0.0.0:9090"));
        assert_eq!(metrics_bind(Some("off".into())), None);
        assert_eq!(metrics_bind(Some("127.0.0.1:9100".into())).as_deref(), Some("127.0.0.1:9100"));
    }
}
