//! Compact JWS verification and JWT claim checks, as jose's `jwtVerify`
//! makes them for Karaka's options. Every failure here is one of jose's
//! `JOSEError`s, which the verifier turns into "no caller".

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::base64::decode_url;
use crate::spki::{Algorithm, PublicKey};

/// jose's options, as Karaka passes them.
pub(crate) struct Options<'a> {
    pub issuer: &'a str,
    pub audience: &'a str,
    pub required: &'a [&'a str],
    pub max_token_age: f64,
    /// Seconds since the epoch, floored, as jose's `epoch(new Date())`.
    pub now: f64,
}

/// Why a token failed: jose's error class, for diagnostics only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoseError {
    JwsInvalid,
    NotSupported,
    AlgNotAllowed,
    SignatureVerificationFailed,
    JwtInvalid,
    ClaimValidationFailed,
    Expired,
}

fn object(bytes: &[u8]) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(&String::from_utf8_lossy(bytes)) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Verifies `token`'s signature with the key its `kid` names, then its
/// claims. (jose: `compactVerify`, then `validateClaimsSet`.)
pub(crate) fn verify(
    token: &str,
    keys: &HashMap<String, PublicKey>,
    options: &Options<'_>,
) -> Result<Map<String, Value>, JoseError> {
    let parts: Vec<&str> = token.split('.').collect();
    let [protected, payload, signature] = parts[..] else {
        return Err(JoseError::JwsInvalid);
    };
    let header = decode_url(protected)
        .and_then(|bytes| object(&bytes))
        .ok_or(JoseError::JwsInvalid)?;
    // `crit` may name only `b64`, which must then be present.
    let b64 = match header.get("crit") {
        None => true,
        Some(Value::Array(names)) if !names.is_empty() => {
            for name in names {
                match name.as_str() {
                    Some("b64") if header.contains_key("b64") => {}
                    Some(_) => return Err(JoseError::NotSupported),
                    None => return Err(JoseError::JwsInvalid),
                }
            }
            match header.get("b64") {
                Some(Value::Bool(b64)) => *b64,
                _ => return Err(JoseError::JwsInvalid),
            }
        }
        Some(_) => return Err(JoseError::JwsInvalid),
    };
    let Some(Value::String(alg)) = header.get("alg") else {
        return Err(JoseError::JwsInvalid);
    };
    let allowed: Vec<Algorithm> = keys.values().map(|key| key.algorithm).collect();
    let Some(algorithm) = Algorithm::from_name(alg).filter(|alg| allowed.contains(alg)) else {
        return Err(JoseError::AlgNotAllowed);
    };
    // Karaka's resolver: the `kid`'s key, for exactly its algorithm.
    let key = header
        .get("kid")
        .and_then(Value::as_str)
        .and_then(|kid| keys.get(kid))
        .filter(|key| key.algorithm == algorithm)
        .ok_or(JoseError::SignatureVerificationFailed)?;
    let signature = decode_url(signature).ok_or(JoseError::JwsInvalid)?;
    let message = format!("{protected}.{payload}");
    if !key.verify(message.as_bytes(), &signature) {
        return Err(JoseError::SignatureVerificationFailed);
    }
    // jose's `jwtVerify`: JWTs MUST NOT use unencoded payload.
    if !b64 {
        return Err(JoseError::JwtInvalid);
    }
    let payload = decode_url(payload).ok_or(JoseError::JwsInvalid)?;
    let claims = object(&payload).ok_or(JoseError::JwtInvalid)?;
    validate(&claims, options)?;
    Ok(claims)
}

fn number(claims: &Map<String, Value>, name: &str) -> Result<Option<f64>, JoseError> {
    match claims.get(name) {
        None => Ok(None),
        Some(Value::Number(value)) => Ok(value.as_f64()),
        Some(_) => Err(JoseError::JwtInvalid),
    }
}

/// (jose: `validateClaimsSet`, with no clock tolerance.)
fn validate(claims: &Map<String, Value>, options: &Options<'_>) -> Result<(), JoseError> {
    // jose adds `iss`, `aud` and `iat` for its issuer, audience and
    // maxTokenAge options.
    for name in options.required.iter().chain(&["iss", "aud", "iat"]) {
        if !claims.contains_key(*name) {
            return Err(JoseError::ClaimValidationFailed);
        }
    }
    if claims.get("iss").and_then(Value::as_str) != Some(options.issuer) {
        return Err(JoseError::ClaimValidationFailed);
    }
    let audience = match claims.get("aud") {
        Some(Value::String(aud)) => aud == options.audience,
        Some(Value::Array(auds)) => auds
            .iter()
            .any(|aud| aud.as_str() == Some(options.audience)),
        _ => false,
    };
    if !audience {
        return Err(JoseError::ClaimValidationFailed);
    }
    let now = options.now;
    let iat = number(claims, "iat")?;
    if let Some(nbf) = number(claims, "nbf")?
        && nbf > now
    {
        return Err(JoseError::ClaimValidationFailed);
    }
    if let Some(exp) = number(claims, "exp")?
        && exp <= now
    {
        return Err(JoseError::Expired);
    }
    let iat = iat.ok_or(JoseError::ClaimValidationFailed)?;
    if iat > now {
        return Err(JoseError::ClaimValidationFailed);
    }
    if now - iat > options.max_token_age {
        return Err(JoseError::Expired);
    }
    Ok(())
}
