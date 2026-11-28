//! Browser credentials: compact JWTs an application server signs for one of
//! its users and one chat, verified here before a browser connection may
//! use them. (Karaka's own: `@karaka-ai/browser-auth`, `src/index.ts`, over
//! jose's `importSPKI` and `jwtVerify`.)
//!
//! A token passes when:
//! - its protected header's `kid` names a configured key, and `alg` is that
//!   key's algorithm: ES256, RS256 or EdDSA (Ed25519);
//! - its signature verifies;
//! - jose's claim checks pass: `iss` and `aud` (a string, or an array
//!   holding it) match, `exp` hasn't passed, `nbf` has, `iat` isn't in the
//!   future and no older than `maxTokenAgeSeconds`, with no clock
//!   tolerance, and the required claims are present;
//! - then Karaka's own: `applicationId` is this deployment's, `tenantId`,
//!   `userId` and `chatId` are non-empty strings, and `exp` is after `iat`
//!   by at most `maxTokenAgeSeconds`.
//!
//! Any of jose's failures is no caller; jose's JOSE errors are the only
//! ones verification makes, so `authenticate` has no error of its own.

mod base64;
mod jwt;
mod spki;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use karaka_identity::{ApplicationId, ApplicationOwner, TenantId, UserId};
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};
use karaka_session::SessionId;
use serde_json::Value;

pub use base64::{encode_standard, encode_url};
pub use jwt::JoseError;
pub use spki::Algorithm;

/// The largest `maxTokenAgeSeconds`. (Karaka: its schema's `max`.)
pub const MAX_TOKEN_AGE_SECONDS: u64 = 2_147_483;

/// A verified browser user, its chat, and when its credential expires.
/// (Karaka: `BrowserCaller`, whose `kind` is always `application`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserCaller {
    pub owner: ApplicationOwner,
    pub chat_id: SessionId,
    /// `exp`, in milliseconds since the epoch.
    pub expires_at: u64,
}

/// One verification key. (Karaka: `Config.keys`'s items.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyConfig {
    pub id: String,
    pub algorithm: Algorithm,
    /// SPKI PEM.
    pub public_key: String,
}

/// (Karaka: `Config`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserAuthConfig {
    pub application_id: String,
    pub issuer: String,
    pub audience: String,
    pub max_token_age_seconds: u64,
    pub keys: Vec<KeyConfig>,
}

impl BrowserAuthConfig {
    /// Reads Karaka's row config: `applicationId`, `issuer`, `audience`,
    /// `maxTokenAgeSeconds` (1 to 2147483) and `keys`, at least one, each
    /// with `id`, `algorithm` and `publicKey`. A key it doesn't know is
    /// refused.
    ///
    /// # Errors
    ///
    /// A missing or misshapen field, said with `at`.
    pub fn from_json(value: &Value, at: &str) -> Result<Self, String> {
        let keys_allowed = [
            "applicationId",
            "issuer",
            "audience",
            "maxTokenAgeSeconds",
            "keys",
        ];
        let Value::Object(map) = value else {
            return Err(format!("{at} must be a mapping"));
        };
        if let Some(key) = map.keys().find(|key| !keys_allowed.contains(&key.as_str())) {
            return Err(format!("{at}: unknown key \"{key}\""));
        }
        let text = |map: &serde_json::Map<String, Value>, key: &str, at: &str| match map.get(key) {
            Some(Value::String(text)) if !text.is_empty() => Ok(text.clone()),
            None | Some(Value::Null) => Err(format!("{at} needs \"{key}\"")),
            Some(_) => Err(format!("{at}: \"{key}\" must be a non-empty string")),
        };
        let age = map
            .get("maxTokenAgeSeconds")
            .ok_or(format!("{at} needs \"maxTokenAgeSeconds\""))?
            .as_u64()
            .filter(|age| (1..=MAX_TOKEN_AGE_SECONDS).contains(age))
            .ok_or(format!(
                "{at}: \"maxTokenAgeSeconds\" must be a whole number from 1 to {MAX_TOKEN_AGE_SECONDS}"
            ))?;
        let Some(Value::Array(rows)) = map.get("keys") else {
            return Err(format!("{at} needs \"keys\", a list"));
        };
        if rows.is_empty() {
            return Err(format!("{at}: \"keys\" must hold at least one key"));
        }
        let mut keys = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let at = format!("{at}: key {}", index + 1);
            let Value::Object(row) = row else {
                return Err(format!("{at} must be a mapping"));
            };
            if let Some(key) = row
                .keys()
                .find(|key| !["id", "algorithm", "publicKey"].contains(&key.as_str()))
            {
                return Err(format!("{at}: unknown key \"{key}\""));
            }
            let algorithm = text(row, "algorithm", &at)?;
            keys.push(KeyConfig {
                id: text(row, "id", &at)?,
                algorithm: Algorithm::from_name(&algorithm).ok_or(format!(
                    "{at}: \"algorithm\" must be ES256, RS256 or EdDSA, not \"{algorithm}\""
                ))?,
                public_key: text(row, "publicKey", &at)?,
            });
        }
        Ok(Self {
            application_id: text(map, "applicationId", at)?,
            issuer: text(map, "issuer", at)?,
            audience: text(map, "audience", at)?,
            max_token_age_seconds: age,
            keys,
        })
    }
}

/// The verifier. (Karaka: `BrowserAuthentication`, the
/// `karakaBrowserAuth` service.)
pub struct BrowserAuthentication {
    config: BrowserAuthConfig,
    keys: HashMap<String, spki::PublicKey>,
}

impl fmt::Debug for BrowserAuthentication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserAuthentication")
            .field("application_id", &self.config.application_id)
            .field("keys", &self.keys.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The required claims. (Karaka: `requiredClaims`.)
const REQUIRED: [&str; 6] = [
    "exp",
    "iat",
    "applicationId",
    "tenantId",
    "userId",
    "chatId",
];

fn non_empty(claims: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
    claims
        .get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

impl BrowserAuthentication {
    /// Imports every key. (Karaka: `apply`.)
    ///
    /// # Errors
    ///
    /// Two keys share an id, in Karaka's words, or a key doesn't import,
    /// in jose's.
    pub fn new(config: BrowserAuthConfig) -> Result<Self, String> {
        let mut keys = HashMap::new();
        for entry in &config.keys {
            if keys.contains_key(&entry.id) {
                return Err("browser-auth: verification key ids must be unique".into());
            }
            let key = spki::import(&entry.public_key, entry.algorithm)
                .map_err(|error| format!("browser-auth: key \"{}\": {error}", entry.id))?;
            keys.insert(entry.id.clone(), key);
        }
        Ok(Self { config, keys })
    }

    /// The caller a compact JWT signs, or `None` for one that doesn't
    /// verify. (Karaka: `authenticate`.)
    pub fn authenticate(&self, credential: &str) -> Option<BrowserCaller> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        self.authenticate_at(credential, now)
    }

    /// As [`authenticate`](Self::authenticate), at `now` seconds since the
    /// epoch.
    pub fn authenticate_at(&self, credential: &str, now: u64) -> Option<BrowserCaller> {
        self.check(credential, now).ok()
    }

    /// As [`authenticate_at`](Self::authenticate_at), saying why a token
    /// failed jose's checks; `Ok(None)` is a failure of Karaka's own.
    ///
    /// # Errors
    ///
    /// jose's error class.
    pub fn check(&self, credential: &str, now: u64) -> Result<BrowserCaller, Option<JoseError>> {
        let config = &self.config;
        let max_age = config.max_token_age_seconds as f64;
        let claims = jwt::verify(
            credential,
            &self.keys,
            &jwt::Options {
                issuer: &config.issuer,
                audience: &config.audience,
                required: &REQUIRED,
                max_token_age: max_age,
                now: now as f64,
            },
        )
        .map_err(Some)?;
        let application = claims.get("applicationId").and_then(Value::as_str);
        let tenant = non_empty(&claims, "tenantId");
        let user = non_empty(&claims, "userId");
        let chat = non_empty(&claims, "chatId");
        let exp = claims.get("exp").and_then(Value::as_f64);
        let iat = claims.get("iat").and_then(Value::as_f64);
        let (Some(tenant), Some(user), Some(chat), Some(exp), Some(iat)) =
            (tenant, user, chat, exp, iat)
        else {
            return Err(None);
        };
        if application != Some(config.application_id.as_str())
            || !exp.is_finite()
            || !iat.is_finite()
            || exp <= iat
            || exp - iat > max_age
        {
            return Err(None);
        }
        Ok(BrowserCaller {
            owner: ApplicationOwner {
                application_id: ApplicationId::new(&config.application_id),
                tenant_id: TenantId::new(tenant),
                user_id: UserId::new(user),
            },
            chat_id: SessionId::new(chat),
            expires_at: (exp * 1000.0) as u64,
        })
    }
}

/// Provides [`BrowserAuthentication`]; its setup imports the keys, so a bad
/// key fails the start. (Karaka: the `browser-auth` row.)
pub struct BrowserAuthPlugin {
    config: BrowserAuthConfig,
}

impl BrowserAuthPlugin {
    pub fn new(config: BrowserAuthConfig) -> Self {
        Self { config }
    }
}

impl Plugin for BrowserAuthPlugin {
    fn name(&self) -> &str {
        "browser-auth"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<BrowserAuthentication>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide(Arc::new(BrowserAuthentication::new(self.config.clone())?));
        Ok(())
    }
}
