//! Origin-side verification of Cloudflare Access JWTs (ADR 0013).
//!
//! watcher's public read surface (the UI shell + `/api`) is gated at the edge by
//! Cloudflare Access (ADR 0013). This module lets the *origin* independently verify
//! the Access-issued JWT as defense-in-depth, so an Access-policy slip, a tunnel
//! misconfig, or direct in-cluster access to the pod can't reach `/api`
//! unauthenticated. The origin only ever **validates** a token — it never mints one.
//!
//! The verifier is deliberately **transport-agnostic**: [`Verifier::verify`] takes a
//! raw token string and checks it against the configured issuer/audience. The axum
//! middleware in [`crate::app`] pulls the token out of the `Cf-Access-Jwt-Assertion`
//! header; the `/mcp` guard (ADR 0019) reuses the same verifier
//! with the token taken from `Authorization: Bearer`.
//!
//! ## Fail-open when unconfigured / on JWKS trouble
//!
//! - **Unconfigured** (`WATCHER_ACCESS_TEAM_DOMAIN` / `WATCHER_ACCESS_AUD` unset):
//!   [`Verifier::from_env`] returns `None` and no enforcement is wired in, so local
//!   dev and non-Access deployments keep working unchanged.
//! - **JWKS refresh failure with a warm cache:** serve the last-known keys (a
//!   Cloudflare certs blip must not take down the whole read surface) and log loudly.
//! - **JWKS unavailable with a cold cache** (never fetched successfully): fail
//!   **open** with a loud warning — the request is allowed and the edge (Cloudflare
//!   Access) remains the primary gate. This trades a strictly-closed posture for
//!   availability, which is the right call for a *defense-in-depth* layer: hard-
//!   failing every request when Cloudflare's certs endpoint is briefly unreachable
//!   would be a worse outage than briefly falling back to edge-only auth.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

/// How often to refresh the cached JWKS. Cloudflare rotates Access signing keys
/// periodically; an hour keeps us well inside the rotation window while sparing the
/// certs endpoint a fetch per request.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3600);

/// Bound the JWKS fetch so a hung certs endpoint can't stall request handling.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Claims we pull off a verified Access token. Cloudflare includes more (`aud`,
/// `iss`, `iat`, `identity_nonce`, …); `iss`/`aud`/`exp` are checked by
/// [`Validation`] against the configured expectations, so here we only surface the
/// caller identity that downstream (e.g. the `/mcp` guard) may want.
#[derive(Debug, Clone, Deserialize)]
pub struct AccessClaims {
    /// Subject — the Access user id.
    #[serde(default)]
    pub sub: String,
    /// The authenticated user's email, when Access includes it.
    #[serde(default)]
    pub email: Option<String>,
    /// Expiry (unix seconds). Present is required and validated.
    pub exp: usize,
}

/// Why a token failed verification.
#[derive(Debug)]
pub enum VerifyError {
    /// The token is present but did not validate (bad signature, wrong `aud`/`iss`,
    /// expired, malformed, or an unknown `kid`). The caller should reject (`401`).
    Invalid(String),
    /// The JWKS could not be obtained and we have never cached keys, so the token
    /// cannot be checked at all. The middleware treats this as fail-open (allow +
    /// warn); a stricter caller may choose to reject.
    KeysUnavailable,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Invalid(why) => write!(f, "invalid Access token: {why}"),
            VerifyError::KeysUnavailable => write!(f, "Access JWKS unavailable"),
        }
    }
}

/// A cached JWKS keyed by `kid`, with the time it was fetched (for TTL refresh).
struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Instant,
}

/// The result of resolving a `kid` against the JWKS.
enum KeyLookup {
    /// The signing key for this `kid` (from a fresh fetch or a warm cache).
    Found(DecodingKey),
    /// Keys are available but none matches this `kid` (unknown key / bad token).
    UnknownKid,
    /// No keys at all — cold cache and the certs endpoint is unreachable. The
    /// middleware treats this as fail-open.
    Unavailable,
}

/// Validates Cloudflare Access JWTs against a team's JWKS.
///
/// Cheaply cloneable-by-`Arc` (the cache lives behind an `Arc<RwLock<…>>`), so the
/// same verifier backs both the axum middleware and any future Bearer-token guard.
pub struct Verifier {
    /// Expected `iss` — the team domain as a URL, e.g. `https://team.cloudflareaccess.com`.
    issuer: String,
    /// Expected `aud` — the Access application's AUD tag.
    audience: String,
    /// Where the signing keys live, e.g. `<issuer>/cdn-cgi/access/certs`.
    certs_url: String,
    http: reqwest::Client,
    cache: Arc<RwLock<Option<KeyCache>>>,
}

/// The shape of Cloudflare's `/cdn-cgi/access/certs` response.
#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: String,
    #[serde(default)]
    kty: String,
    /// RSA modulus (base64url).
    n: String,
    /// RSA exponent (base64url).
    e: String,
}

impl Verifier {
    /// Construct a verifier for an explicit issuer / certs URL / audience. Used by
    /// [`from_env`](Self::from_env) and by tests that point at a local JWKS server.
    pub fn new(
        issuer: impl Into<String>,
        certs_url: impl Into<String>,
        audience: impl Into<String>,
    ) -> Self {
        Verifier {
            issuer: issuer.into(),
            certs_url: certs_url.into(),
            audience: audience.into(),
            http: reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .build()
                .expect("reqwest client"),
            cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Build a verifier from a Cloudflare Access **team domain** and an expected
    /// audience, deriving the issuer and JWKS (`certs`) URLs the way Cloudflare does.
    ///
    /// The team domain may be given with or without a scheme
    /// (`team.cloudflareaccess.com`); Cloudflare's `iss` is that host with an
    /// `https://` scheme and no trailing slash. Shared by [`from_env`](Self::from_env)
    /// (the browser Access app) and the `/mcp` Bearer guard (ADR 0019), which points at
    /// the same team but a **separate** Access application AUD.
    pub fn for_team(team_domain: &str, audience: impl Into<String>) -> Self {
        let host = team_domain
            .trim()
            .trim_end_matches('/')
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        let issuer = format!("https://{host}");
        let certs_url = format!("{issuer}/cdn-cgi/access/certs");
        Self::new(issuer, certs_url, audience)
    }

    /// The expected issuer (`iss`) — the team domain as an `https://` URL. Also the
    /// Cloudflare Access OIDC authorization-server identifier.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Build a verifier from the environment, or `None` when Access verification is
    /// not configured (so enforcement is simply not wired in — see the module docs).
    ///
    /// `WATCHER_ACCESS_TEAM_DOMAIN` is the team domain (`team.cloudflareaccess.com`,
    /// with or without a scheme); `WATCHER_ACCESS_AUD` is the Access application's
    /// AUD tag. Both must be non-empty to enforce.
    pub fn from_env() -> Option<Self> {
        let team = std::env::var("WATCHER_ACCESS_TEAM_DOMAIN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        let audience = std::env::var("WATCHER_ACCESS_AUD")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        Some(Self::for_team(&team, audience))
    }

    /// Verify a raw JWT string against the configured issuer/audience: signature
    /// (RS256), `iss`, `aud`, and expiry are all checked. Returns the token's claims
    /// on success. Transport-agnostic — the caller supplies the token however it
    /// arrived (Access header, Bearer, …).
    pub async fn verify(&self, token: &str) -> Result<AccessClaims, VerifyError> {
        let header = decode_header(token)
            .map_err(|e| VerifyError::Invalid(format!("malformed JWT header: {e}")))?;
        let kid = header
            .kid
            .ok_or_else(|| VerifyError::Invalid("JWT header missing kid".into()))?;

        // Resolve the signing key for this kid. On a miss, force one refresh in case
        // Access rotated keys since our last fetch before giving up.
        let key = match self.lookup(&kid, false).await {
            KeyLookup::Found(key) => key,
            KeyLookup::UnknownKid => match self.lookup(&kid, true).await {
                KeyLookup::Found(key) => key,
                KeyLookup::UnknownKid => {
                    return Err(VerifyError::Invalid("JWT kid not present in JWKS".into()))
                }
                KeyLookup::Unavailable => return Err(VerifyError::KeysUnavailable),
            },
            KeyLookup::Unavailable => return Err(VerifyError::KeysUnavailable),
        };

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.audience.as_str()]);
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        decode::<AccessClaims>(token, &key, &validation)
            .map(|data| data.claims)
            .map_err(|e| VerifyError::Invalid(e.to_string()))
    }

    /// Resolve one `kid` to its decoding key, refreshing the JWKS per the TTL (or
    /// unconditionally when `force`) and cloning only the matched key. On a refresh
    /// failure: fall back to the last-known warm cache if we have one (log loudly),
    /// else report `Unavailable` (cold cache → fail-open).
    async fn lookup(&self, kid: &str, force: bool) -> KeyLookup {
        if !force {
            let guard = self.cache.read().await;
            if let Some(cache) = guard.as_ref() {
                if cache.fetched_at.elapsed() < REFRESH_INTERVAL {
                    return match cache.keys.get(kid) {
                        Some(key) => KeyLookup::Found(key.clone()),
                        None => KeyLookup::UnknownKid,
                    };
                }
            }
        }

        match self.fetch_keys().await {
            Ok(keys) => {
                let found = keys.get(kid).cloned();
                *self.cache.write().await = Some(KeyCache {
                    keys,
                    fetched_at: Instant::now(),
                });
                match found {
                    Some(key) => KeyLookup::Found(key),
                    None => KeyLookup::UnknownKid,
                }
            }
            Err(e) => {
                let guard = self.cache.read().await;
                match guard.as_ref() {
                    Some(cache) => {
                        tracing::warn!(
                            "Cloudflare Access JWKS refresh failed ({e:#}); serving last-known keys"
                        );
                        match cache.keys.get(kid) {
                            Some(key) => KeyLookup::Found(key.clone()),
                            None => KeyLookup::UnknownKid,
                        }
                    }
                    None => {
                        tracing::warn!(
                            "Cloudflare Access JWKS unavailable and no cached keys ({e:#}); \
                             failing OPEN — origin verification skipped, edge remains the gate"
                        );
                        KeyLookup::Unavailable
                    }
                }
            }
        }
    }

    /// Fetch and parse the JWKS, turning each RSA entry into a decoding key.
    async fn fetch_keys(&self) -> anyhow::Result<HashMap<String, DecodingKey>> {
        let jwks: Jwks = self
            .http
            .get(&self.certs_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut keys = HashMap::new();
        for jwk in jwks.keys {
            if jwk.kty != "RSA" {
                continue;
            }
            match DecodingKey::from_rsa_components(&jwk.n, &jwk.e) {
                Ok(key) => {
                    keys.insert(jwk.kid, key);
                }
                Err(e) => tracing::warn!("skipping malformed JWK {}: {e}", jwk.kid),
            }
        }
        if keys.is_empty() {
            anyhow::bail!("JWKS contained no usable RSA keys");
        }
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    // A throwaway RSA-2048 keypair generated for tests only (never a real key). The
    // JWK modulus/exponent below correspond to this private PEM.
    const TEST_KID: &str = "test-key-1";
    const TEST_N: &str = "uvr8rE8LT_sYjwq02YqlXZNFbHga1O3uxDiBLr7J39ELOGtLeTtl6QZF4NNJEufj_nQso32EPIffObihmofqAxiiU_JctOt0IH_Cfbbn5aVnQidUhtzo7URe_neZ4fT8lqtUyPHBcKt1Vt2p9igpntQH0hrfUAnCMXiCh9te0bgBjtV4NjtBlwhZGD8rohumJcMN8Q12gHJNsmhRIym5hvQMeth7nuff7u4Ttr6kAZ90TU57PSgUOT12pbx-UT2yiyaJUv6xMSjIhc4og-wGNRJZ7R-1Zb3WVj5Je_6bvwrA6hwgo7nxXSmKsbmoXpaWBkpxJq0uUNqG09a5-Ivhiw";
    const TEST_E: &str = "AQAB";
    const TEST_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC6+vysTwtP+xiP
CrTZiqVdk0VseBrU7e7EOIEuvsnf0Qs4a0t5O2XpBkXg00kS5+P+dCyjfYQ8h985
uKGah+oDGKJT8ly063Qgf8J9tuflpWdCJ1SG3OjtRF7+d5nh9PyWq1TI8cFwq3VW
3an2KCme1AfSGt9QCcIxeIKH217RuAGO1Xg2O0GXCFkYPyuiG6Ylww3xDXaAck2y
aFEjKbmG9Ax62Hue59/u7hO2vqQBn3RNTns9KBQ5PXalvH5RPbKLJolS/rExKMiF
ziiD7AY1ElntH7VlvdZWPkl7/pu/CsDqHCCjufFdKYqxuahelpYGSnEmrS5Q2obT
1rn4i+GLAgMBAAECggEABbkk/sk0mXAgIlC7lGUQBrs5RsauW5Ik2tC384xXdYha
hZGTL9THm8hbXzRYakG60tEPhLmU0J2AEa47FBXQ7eNVJKioeckzNsNyWpK8qmTT
skyt46rjXk/XcIaMqUPsb1gzMitkNmSpJM2IJEa6b2giDSZRa4vA6+66YBow3s5r
pq9WxymTWinSmrkPjiH0dQ4X5O9B8VK88ITZZHEbHmN6g7RXnVoVknGyCgV19jEr
k+iUz5Gmth9PD6tiA7fIhcKcAEyXwCRaWe1dfdWynm2gfHgjhmDME+B3BsiL14DU
lZ/VuCLcgGky+SdxqG9OsYgoBFygsiGC2DSab+Ja8QKBgQDi/w1IYDoibbaPDO+n
pm5SAa3vv7snlEcDocNUROcgUGN5qr9Kek4Me4Cgtx/RYdaf/P3SODLV47V7yI4x
dAQZlqPaKd4ogwJTdr+Bh97if+8gOYprhmZRqsIomCZrNe1f7gjP2GLyIa8FnfD2
GERliRxE3hHj9SSGfWGRXx7hfQKBgQDS3wiTDuBi1PE2cvp8rytW0dOoJKsEk0K8
wpHbRIumzhkCMwUdh2o8B46RaFSNPqaoRpiLZJykM5UZ4LKNTEcYHrwkc9W+cOF7
JhCDWpdrKDeHBnRdNEs1dhJYoIRkmNp/C1YD+PPie9X46Jib1F0FP04ihMJ2KWdD
wXimXhw9pwKBgQDWCtQWjA4lSrja+MK+nhPmpgjCSlOKxamUxiLuQi6CbOrv3c6U
xvDzmj02zpZlFFGR+LfKUw20XAxUFU/nV9NJ4Z7NZ69BGg/GbfG0jU7g2uu7wiZA
r7Gpjk+YgaewbmBPlZ+fhRX/5T0pGb4N/+H2sCwE0DWkcxKm8nFe54ex7QKBgQDF
dDD8OwbjpI/Fs35X+FK1tj7iCIvW+emZBPw8/H9kD0Kdq5aTovRYB595Ct95bvvx
QEGg7PI8U0y/cYbgBlff/w+fdpPkAqEwhmEaDl8Q6RStq96UU95Ezi25rXyrEfIu
2jeN+rSsE9c1ft8/s2fy/Oc2LWhF6tkWOfi2mBMLqwKBgBsBQelfPVHUCmI1Qsyr
hiyFRxn1pJdezc0RjETBfNsDKTJfuEcAxI4X9Z1iTgGLiJrbO5FuNhRN5ShfnrdU
rF7q76B1WCT69QpyJ+OHl0Sp6Uegf09l3QeJE6eTxDS9r7qGFPoAn5v5zBpsrrhF
lsCu2w3KPmCX769JvIYnwU8y
-----END PRIVATE KEY-----";

    const ISSUER: &str = "https://team.cloudflareaccess.com";
    const AUD: &str = "test-aud-tag";

    fn now() -> usize {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as usize
    }

    /// Sign a JWT with the test key and the given claims.
    fn sign(claims: serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(TEST_KID.to_string());
        let key = EncodingKey::from_rsa_pem(TEST_PEM.as_bytes()).unwrap();
        encode(&header, &claims, &key).unwrap()
    }

    fn valid_claims() -> serde_json::Value {
        json!({
            "iss": ISSUER,
            "aud": [AUD],
            "sub": "user-123",
            "email": "u@example.com",
            "exp": now() + 3600,
            "iat": now(),
        })
    }

    /// A verifier whose cache is pre-seeded with the test key, so `verify` never
    /// touches the network. `certs_url` points nowhere — a cache miss/forced refresh
    /// would fail, which is exactly what the kid/keys-unavailable tests want.
    async fn seeded_verifier() -> Verifier {
        let v = Verifier::new(ISSUER, "http://127.0.0.1:1/certs", AUD);
        let mut keys = HashMap::new();
        keys.insert(
            TEST_KID.to_string(),
            DecodingKey::from_rsa_components(TEST_N, TEST_E).unwrap(),
        );
        v.cache.write().await.replace(KeyCache {
            keys,
            fetched_at: Instant::now(),
        });
        v
    }

    #[tokio::test]
    async fn valid_token_passes() {
        let v = seeded_verifier().await;
        let claims = v.verify(&sign(valid_claims())).await.expect("valid");
        assert_eq!(claims.sub, "user-123");
        assert_eq!(claims.email.as_deref(), Some("u@example.com"));
    }

    #[tokio::test]
    async fn wrong_audience_rejected() {
        let v = seeded_verifier().await;
        let mut c = valid_claims();
        c["aud"] = json!(["some-other-app"]);
        assert!(matches!(
            v.verify(&sign(c)).await,
            Err(VerifyError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn wrong_issuer_rejected() {
        let v = seeded_verifier().await;
        let mut c = valid_claims();
        c["iss"] = json!("https://evil.cloudflareaccess.com");
        assert!(matches!(
            v.verify(&sign(c)).await,
            Err(VerifyError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn expired_token_rejected() {
        let v = seeded_verifier().await;
        let mut c = valid_claims();
        // Well past jsonwebtoken's default 60s expiry leeway.
        c["exp"] = json!(now() - 3600);
        assert!(matches!(
            v.verify(&sign(c)).await,
            Err(VerifyError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn tampered_signature_rejected() {
        let v = seeded_verifier().await;
        let mut token = sign(valid_claims());
        // Flip the last character of the signature segment.
        let last = token.pop().unwrap();
        token.push(if last == 'A' { 'B' } else { 'A' });
        assert!(matches!(
            v.verify(&token).await,
            Err(VerifyError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn wrong_algorithm_rejected() {
        // Classic alg-confusion attempt: sign with HS256 (e.g. treating the RSA
        // public key material as an HMAC secret) instead of the RS256 the verifier
        // requires. `Validation::new(Algorithm::RS256)` allowlists only RS256, so
        // `decode` must reject this by header alone, before any signature check.
        let v = seeded_verifier().await;
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(TEST_KID.to_string());
        let key = EncodingKey::from_secret(TEST_N.as_bytes());
        let token = encode(&header, &valid_claims(), &key).unwrap();
        assert!(matches!(
            v.verify(&token).await,
            Err(VerifyError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn unknown_kid_rejected() {
        let v = seeded_verifier().await;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("nope".to_string());
        let key = EncodingKey::from_rsa_pem(TEST_PEM.as_bytes()).unwrap();
        let token = encode(&header, &valid_claims(), &key).unwrap();
        // kid miss forces a refresh against the dead certs_url, but the warm cache
        // means this is a genuine unknown-kid rejection, not KeysUnavailable.
        assert!(matches!(
            v.verify(&token).await,
            Err(VerifyError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn garbage_token_rejected() {
        let v = seeded_verifier().await;
        assert!(matches!(
            v.verify("not-a-jwt").await,
            Err(VerifyError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn cold_cache_with_dead_jwks_is_unavailable() {
        // No seeded cache and an unreachable certs endpoint → fail-open signal.
        let v = Verifier::new(ISSUER, "http://127.0.0.1:1/certs", AUD);
        assert!(matches!(
            v.verify(&sign(valid_claims())).await,
            Err(VerifyError::KeysUnavailable)
        ));
    }
}
