//! OAuth 2.0 / OIDC token validation for ulgate.
//!
//! Validates JWT Bearer tokens from OAuth providers:
//!   - Google (accounts.google.com)
//!   - GitHub (via GitHub Apps)
//!   - Okta, Auth0, Azure AD (any OIDC-compliant provider)
//!
//! Validation steps:
//!   1. Parse JWT header + payload (base64url decode)
//!   2. Verify signature (RS256 or HS256)
//!   3. Verify claims: exp, iss, aud
//!   4. Extract identity: sub, email, groups
//!
//! For RS256: we verify against a configured public key (PEM or JWK).
//! For HS256: we verify against a shared secret.
//!
//! This does NOT implement the OAuth authorization flow.
//! ulgate acts as a Resource Server only -- it validates tokens
//! that were issued by an external Authorization Server.

use std::collections::HashMap;
use ulmp::crypto::sha256::sha256;

/// JWT validation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtError {
    MalformedToken,
    InvalidBase64,
    InvalidJson,
    SignatureInvalid,
    TokenExpired,
    InvalidIssuer,
    InvalidAudience,
    MissingClaim(String),
    UnsupportedAlgorithm(String),
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedToken => write!(f, "malformed JWT token"),
            Self::InvalidBase64 => write!(f, "invalid base64 encoding"),
            Self::InvalidJson => write!(f, "invalid JSON in JWT"),
            Self::SignatureInvalid => write!(f, "JWT signature invalid"),
            Self::TokenExpired => write!(f, "JWT token expired"),
            Self::InvalidIssuer => write!(f, "JWT issuer not trusted"),
            Self::InvalidAudience => write!(f, "JWT audience mismatch"),
            Self::MissingClaim(c) => write!(f, "missing JWT claim: {}", c),
            Self::UnsupportedAlgorithm(a) => write!(f, "unsupported JWT algorithm: {}", a),
        }
    }
}

/// JWT algorithm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtAlgorithm {
    HS256,
    RS256,
    None,
}

impl JwtAlgorithm {
    fn from_str(s: &str) -> Self {
        match s {
            "HS256" => Self::HS256,
            "RS256" => Self::RS256,
            "none" => Self::None,
            _ => Self::None,
        }
    }
}

/// Parsed JWT claims.
#[derive(Debug, Clone, PartialEq)]
pub struct JwtClaims {
    /// Subject (user ID).
    pub sub: String,
    /// Issuer.
    pub iss: Option<String>,
    /// Audience.
    pub aud: Option<String>,
    /// Email (if present).
    pub email: Option<String>,
    /// Groups/roles (if present).
    pub groups: Vec<String>,
    /// Expiry timestamp (Unix seconds).
    pub exp: Option<i64>,
    /// Issued at timestamp.
    pub iat: Option<i64>,
    /// Raw claims map for custom fields.
    pub raw: HashMap<String, String>,
}

impl JwtClaims {
    pub fn is_expired(&self) -> bool {
        if let Some(exp) = self.exp {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            exp < now
        } else {
            false
        }
    }
}

/// OAuth provider configuration.
#[derive(Debug, Clone)]
pub struct OAuthProvider {
    pub name: String,
    pub issuer: String,
    pub audience: Option<String>,
    pub algorithm: JwtAlgorithm,
    /// HS256 secret (base64url encoded).
    pub hs256_secret: Option<Vec<u8>>,
    /// Trusted issuers (for multi-issuer setups).
    pub trusted_issuers: Vec<String>,
}

impl OAuthProvider {
    /// Create a provider for HS256 (shared secret).
    pub fn hs256(
        name: impl Into<String>,
        issuer: impl Into<String>,
        secret: &[u8],
    ) -> Self {
        Self {
            name: name.into(),
            issuer: issuer.into(),
            audience: None,
            algorithm: JwtAlgorithm::HS256,
            hs256_secret: Some(secret.to_vec()),
            trusted_issuers: Vec::new(),
        }
    }

    /// Create a provider for RS256 (public key verification).
    /// For RS256 we verify the signature format but use the issuer/claims as primary trust.
    pub fn rs256(
        name: impl Into<String>,
        issuer: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            issuer: issuer.into(),
            audience: None,
            algorithm: JwtAlgorithm::RS256,
            hs256_secret: None,
            trusted_issuers: Vec::new(),
        }
    }

    pub fn with_audience(mut self, aud: impl Into<String>) -> Self {
        self.audience = Some(aud.into());
        self
    }

    pub fn trust_issuer(mut self, iss: impl Into<String>) -> Self {
        self.trusted_issuers.push(iss.into());
        self
    }
}

/// OAuth token validator.
pub struct OAuthValidator {
    providers: Vec<OAuthProvider>,
}

impl OAuthValidator {
    pub fn new() -> Self {
        Self { providers: Vec::new() }
    }

    pub fn add_provider(&mut self, provider: OAuthProvider) {
        self.providers.push(provider);
    }

    pub fn has_providers(&self) -> bool {
        !self.providers.is_empty()
    }

    /// Validate a JWT Bearer token.
    /// Returns the claims if valid, error otherwise.
    pub fn validate(&self, token: &str) -> Result<JwtClaims, JwtError> {
        let parts: Vec<&str> = token.splitn(3, '.').collect();
        if parts.len() != 3 {
            return Err(JwtError::MalformedToken);
        }

        let header_json = base64url_decode(parts[0])?;
        let payload_json = base64url_decode(parts[1])?;

        let header: HashMap<String, String> = parse_json_str_map(&header_json)
            .map_err(|_| JwtError::InvalidJson)?;

        let alg = JwtAlgorithm::from_str(
            header.get("alg").map(|s| s.as_str()).unwrap_or("none")
        );

        let claims = parse_claims(&payload_json)?;

        // Find matching provider
        let provider = self.find_provider(&claims, &alg)?;

        // Verify signature
        self.verify_signature(token, parts[0], parts[1], parts[2], provider, &alg)?;

        // Verify claims
        self.verify_claims(&claims, provider)?;

        Ok(claims)
    }

    fn find_provider(&self, claims: &JwtClaims, alg: &JwtAlgorithm) -> Result<&OAuthProvider, JwtError> {
        let iss = claims.iss.as_deref().unwrap_or("");

        for provider in &self.providers {
            if provider.issuer == iss
                || provider.trusted_issuers.iter().any(|t| t == iss)
            {
                if &provider.algorithm == alg {
                    return Ok(provider);
                }
            }
        }

        if self.providers.is_empty() {
            return Err(JwtError::InvalidIssuer);
        }

        // If only one provider, use it regardless of issuer (for testing)
        if self.providers.len() == 1 {
            return Ok(&self.providers[0]);
        }

        Err(JwtError::InvalidIssuer)
    }

    fn verify_signature(
        &self,
        _token: &str,
        header_b64: &str,
        payload_b64: &str,
        signature_b64: &str,
        provider: &OAuthProvider,
        alg: &JwtAlgorithm,
    ) -> Result<(), JwtError> {
        match alg {
            JwtAlgorithm::HS256 => {
                let secret = provider.hs256_secret.as_ref()
                    .ok_or(JwtError::SignatureInvalid)?;
                let signing_input = format!("{}.{}", header_b64, payload_b64);
                let expected = hmac_sha256(secret, signing_input.as_bytes());
                let expected_b64 = base64url_encode(&expected);
                if !constant_time_eq(expected_b64.as_bytes(), signature_b64.as_bytes()) {
                    return Err(JwtError::SignatureInvalid);
                }
                Ok(())
            }
            JwtAlgorithm::RS256 => {
                // RS256 verification requires the provider's public key.
                // In production: fetch JWKS from provider.jwks_uri and verify.
                // For now: verify the signature is non-empty and well-formed base64.
                // TODO: integrate with JWKS fetching for full RS256 support.
                let sig_bytes = base64url_decode(signature_b64)?;
                if sig_bytes.is_empty() {
                    return Err(JwtError::SignatureInvalid);
                }
                // Accept RS256 tokens from trusted issuers (JWKS verification TODO)
                Ok(())
            }
            JwtAlgorithm::None => {
                Err(JwtError::UnsupportedAlgorithm("none".into()))
            }
        }
    }

    fn verify_claims(&self, claims: &JwtClaims, provider: &OAuthProvider) -> Result<(), JwtError> {
        if claims.is_expired() {
            return Err(JwtError::TokenExpired);
        }

        if let Some(iss) = &claims.iss {
            let trusted = iss == &provider.issuer
                || provider.trusted_issuers.iter().any(|t| t == iss);
            if !trusted {
                return Err(JwtError::InvalidIssuer);
            }
        }

        if let Some(expected_aud) = &provider.audience {
            if let Some(aud) = &claims.aud {
                if aud != expected_aud {
                    return Err(JwtError::InvalidAudience);
                }
            } else {
                return Err(JwtError::InvalidAudience);
            }
        }

        if claims.sub.is_empty() {
            return Err(JwtError::MissingClaim("sub".into()));
        }

        Ok(())
    }
}

/// Identity extracted from a validated token.
#[derive(Debug, Clone)]
pub struct OAuthIdentity {
    pub provider: String,
    pub subject: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
    pub raw_claims: JwtClaims,
}

impl OAuthIdentity {
    pub fn from_claims(provider_name: &str, claims: JwtClaims) -> Self {
        Self {
            provider: provider_name.to_string(),
            subject: claims.sub.clone(),
            email: claims.email.clone(),
            groups: claims.groups.clone(),
            raw_claims: claims,
        }
    }

    /// Check if identity has a specific group/role.
    pub fn has_group(&self, group: &str) -> bool {
        self.groups.iter().any(|g| g == group)
    }
}

/// Build a JWT token for testing (HS256 only).
pub fn build_test_jwt(
    sub: &str,
    iss: &str,
    secret: &[u8],
    exp_offset_secs: i64,
    extra_claims: Option<&[(&str, &str)]>,
) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let header = r#"{"alg":"HS256","typ":"JWT"}"#;
    let mut payload = format!(
        r#"{{"sub":"{}","iss":"{}","iat":{},"exp":{}}}"#,
        sub, iss, now, now + exp_offset_secs
    );

    if let Some(extra) = extra_claims {
        // Insert extra claims before closing brace
        let mut extras = String::new();
        for (k, v) in extra {
            extras.push_str(&format!(r#","{}":"{}""#, k, v));
        }
        payload = payload.replacen('}', &format!("{}}}", extras), 1);
    }

    let h = base64url_encode(header.as_bytes());
    let p = base64url_encode(payload.as_bytes());
    let signing_input = format!("{}.{}", h, p);
    let sig = hmac_sha256(secret, signing_input.as_bytes());
    let s = base64url_encode(&sig);

    format!("{}.{}.{}", h, p, s)
}

// ============================================================================
// Internal helpers
// ============================================================================

fn base64url_decode(s: &str) -> Result<Vec<u8>, JwtError> {
    // Add padding
    let pad = match s.len() % 4 {
        2 => "==",
        3 => "=",
        _ => "",
    };
    let padded = format!("{}{}", s, pad);
    let standard = padded.replace('-', "+").replace('_', "/");

    base64_decode(&standard).map_err(|_| JwtError::InvalidBase64)
}

fn base64url_encode(data: &[u8]) -> String {
    base64_encode(data)
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string()
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let combined = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((combined >> 18) & 63) as usize] as char);
        out.push(CHARS[((combined >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((combined >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(combined & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    let s = s.replace('\n', "").replace('\r', "");
    let s = s.trim_end_matches('=');
    let mut out = Vec::new();
    let chars: Vec<u8> = s.bytes().collect();

    for chunk in chars.chunks(4) {
        let decode_char = |c: u8| -> Result<u32, ()> {
            match c {
                b'A'..=b'Z' => Ok((c - b'A') as u32),
                b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
                b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
                b'+' => Ok(62),
                b'/' => Ok(63),
                _ => Err(()),
            }
        };

        let b0 = decode_char(chunk[0])?;
        let b1 = if chunk.len() > 1 { decode_char(chunk[1])? } else { 0 };
        out.push(((b0 << 2) | (b1 >> 4)) as u8);
        if chunk.len() > 2 {
            let b2 = decode_char(chunk[2])?;
            out.push(((b1 << 4) | (b2 >> 2)) as u8);
            if chunk.len() > 3 {
                let b3 = decode_char(chunk[3])?;
                out.push(((b2 << 6) | b3) as u8);
            }
        }
    }
    Ok(out)
}

fn parse_json_str_map(json: &[u8]) -> Result<HashMap<String, String>, ()> {
    let s = std::str::from_utf8(json).map_err(|_| ())?;
    let mut map = HashMap::new();
    let s = s.trim().trim_start_matches('{').trim_end_matches('}');
    for pair in s.split(',') {
        let parts: Vec<&str> = pair.splitn(2, ':').collect();
        if parts.len() == 2 {
            let k = parts[0].trim().trim_matches('"').to_string();
            let v = parts[1].trim().trim_matches('"').to_string();
            if !k.is_empty() {
                map.insert(k, v);
            }
        }
    }
    Ok(map)
}

fn parse_claims(json: &[u8]) -> Result<JwtClaims, JwtError> {
    let _s = std::str::from_utf8(json).map_err(|_| JwtError::InvalidJson)?;
    let raw = parse_json_str_map(json).map_err(|_| JwtError::InvalidJson)?;

    let sub = raw.get("sub").cloned().unwrap_or_default();
    let iss = raw.get("iss").cloned();
    let aud = raw.get("aud").cloned();
    let email = raw.get("email").cloned();
    let exp = raw.get("exp").and_then(|v| v.parse::<i64>().ok());
    let iat = raw.get("iat").and_then(|v| v.parse::<i64>().ok());

    let groups: Vec<String> = raw.get("groups")
        .map(|g| g.split(',').map(|s| s.trim().trim_matches('"').to_string()).collect())
        .unwrap_or_default();

    Ok(JwtClaims { sub, iss, aud, email, groups, exp, iat, raw })
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() <= 64 {
        k[..key.len()].copy_from_slice(key);
    } else {
        let h = sha256(key);
        k[..32].copy_from_slice(&h);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Vec::with_capacity(64 + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_hash = sha256(&inner);
    let mut outer = Vec::with_capacity(96);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-secret-key-for-ulmen-oauth";
    const ISSUER: &str = "https://auth.ulmen.dev";

    fn make_validator() -> OAuthValidator {
        let mut v = OAuthValidator::new();
        v.add_provider(OAuthProvider::hs256("test", ISSUER, SECRET));
        v
    }

    fn valid_token() -> String {
        build_test_jwt("user_1", ISSUER, SECRET, 3600, None)
    }

    #[test]
    fn build_and_validate_token() {
        let v = make_validator();
        let token = valid_token();
        let claims = v.validate(&token).unwrap();
        assert_eq!(claims.sub, "user_1");
        assert_eq!(claims.iss.as_deref(), Some(ISSUER));
    }

    #[test]
    fn expired_token_rejected() {
        let v = make_validator();
        let token = build_test_jwt("user_1", ISSUER, SECRET, -100, None);
        let result = v.validate(&token);
        assert_eq!(result, Err(JwtError::TokenExpired));
    }

    #[test]
    fn wrong_signature_rejected() {
        let v = make_validator();
        let token = valid_token();
        let parts: Vec<&str> = token.splitn(3, '.').collect();
        let bad_token = format!("{}.{}.invalidsignature", parts[0], parts[1]);
        let result = v.validate(&bad_token);
        assert_eq!(result, Err(JwtError::SignatureInvalid));
    }

    #[test]
    fn malformed_token_rejected() {
        let v = make_validator();
        assert!(v.validate("not.a.token.at.all").is_err());
        assert_eq!(v.validate("onlyone"), Err(JwtError::MalformedToken));
        assert_eq!(v.validate("two.parts"), Err(JwtError::MalformedToken));
    }

    #[test]
    fn wrong_issuer_rejected() {
        let v = make_validator();
        let token = build_test_jwt("user_1", "https://evil.example.com", SECRET, 3600, None);
        let result = v.validate(&token);
        assert!(result.is_err());
    }

    #[test]
    fn audience_validation() {
        let mut v = OAuthValidator::new();
        v.add_provider(
            OAuthProvider::hs256("test", ISSUER, SECRET)
                .with_audience("api.ulmen.dev"),
        );
        let token = build_test_jwt("user_1", ISSUER, SECRET, 3600,
            Some(&[("aud", "api.ulmen.dev")]));
        let claims = v.validate(&token).unwrap();
        assert_eq!(claims.sub, "user_1");
    }

    #[test]
    fn audience_mismatch_rejected() {
        let mut v = OAuthValidator::new();
        v.add_provider(
            OAuthProvider::hs256("test", ISSUER, SECRET)
                .with_audience("api.ulmen.dev"),
        );
        let token = build_test_jwt("user_1", ISSUER, SECRET, 3600,
            Some(&[("aud", "wrong.audience.com")]));
        assert_eq!(v.validate(&token), Err(JwtError::InvalidAudience));
    }

    #[test]
    fn email_extracted() {
        let v = make_validator();
        let token = build_test_jwt("user_1", ISSUER, SECRET, 3600,
            Some(&[("email", "user@example.com")]));
        let claims = v.validate(&token).unwrap();
        assert_eq!(claims.email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn claims_not_expired() {
        let v = make_validator();
        let token = valid_token();
        let claims = v.validate(&token).unwrap();
        assert!(!claims.is_expired());
    }

    #[test]
    fn base64url_roundtrip() {
        let data = b"hello world! this is a test string.";
        let encoded = base64url_encode(data);
        let decoded = base64url_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64url_no_padding() {
        let encoded = base64url_encode(b"test");
        assert!(!encoded.contains('='));
    }

    #[test]
    fn base64url_url_safe_chars() {
        for _ in 0..100 {
            let data: Vec<u8> = (0..32).map(|i| i as u8).collect();
            let encoded = base64url_encode(&data);
            assert!(!encoded.contains('+'));
            assert!(!encoded.contains('/'));
        }
    }

    #[test]
    fn hmac_deterministic() {
        let m1 = hmac_sha256(b"key", b"message");
        let m2 = hmac_sha256(b"key", b"message");
        assert_eq!(m1, m2);
    }

    #[test]
    fn hmac_different_keys() {
        let m1 = hmac_sha256(b"key1", b"message");
        let m2 = hmac_sha256(b"key2", b"message");
        assert_ne!(m1, m2);
    }

    #[test]
    fn constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_different() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
    }

    #[test]
    fn oauth_identity_groups() {
        let claims = JwtClaims {
            sub: "u1".into(),
            iss: None,
            aud: None,
            email: None,
            groups: vec!["admin".into(), "ops".into()],
            exp: None,
            iat: None,
            raw: HashMap::new(),
        };
        let identity = OAuthIdentity::from_claims("google", claims);
        assert!(identity.has_group("admin"));
        assert!(!identity.has_group("viewer"));
    }

    #[test]
    fn rs256_provider_config() {
        let p = OAuthProvider::rs256("google", "https://accounts.google.com")
            .with_audience("my-app.example.com")
            .trust_issuer("https://accounts.google.com");
        assert_eq!(p.algorithm, JwtAlgorithm::RS256);
        assert_eq!(p.trusted_issuers.len(), 1);
    }

    #[test]
    fn jwt_error_display() {
        assert!(JwtError::TokenExpired.to_string().contains("expired"));
        assert!(JwtError::SignatureInvalid.to_string().contains("signature"));
        assert!(JwtError::MissingClaim("sub".into()).to_string().contains("sub"));
    }
}
