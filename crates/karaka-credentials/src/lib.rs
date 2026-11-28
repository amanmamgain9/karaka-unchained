//! Credentials by reference: configuration names a secret, and a provider
//! resolves the name when it's needed, so a rotated value is read on the
//! next use. (dsh: `@deepseek-ai/dsh-credentials`,
//! `credentials/credentials/src/index.ts`.)
//!
//! A reference is a POSIX environment-variable name. dsh's local provider
//! (`credentials-local`) resolves one from its own store, then the process
//! environment, then the project's and the user's `.env` files. Only the
//! process environment is ported, as [`EnvCredentials`]; the store, the
//! `.env` layers and editing (`describe`, `set`, `unset`, change events)
//! wait for `credentials-local`.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};

/// What a reference must match, as dsh writes it in its error.
/// (dsh: `REF_PATTERN`.)
pub const REF_PATTERN: &str = "/^[A-Za-z_][A-Za-z0-9_]*$/";

/// The name of one credential. (dsh: `CredentialRef`.)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// (dsh: `credentialRef`.)
    ///
    /// # Errors
    ///
    /// `value` isn't an environment-variable name, in dsh's words.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialRefError> {
        let value = value.into();
        if is_ref_name(&value) {
            Ok(Self(value))
        } else {
            Err(CredentialRefError { value })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// (dsh: `isCredentialRefName`.)
fn is_ref_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|rest| rest.is_ascii_alphanumeric() || rest == '_')
}

/// A name that isn't a reference. (dsh: the `TypeError` `credentialRef`
/// throws.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialRefError {
    pub value: String,
}

impl fmt::Display for CredentialRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "credential ref \"{}\" must match {REF_PATTERN}",
            self.value
        )
    }
}

impl std::error::Error for CredentialRefError {}

/// One resolved value, and the layer that gave it. (dsh:
/// `ResolvedCredential`.)
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedCredential {
    /// Never empty.
    pub value: String,
    /// The provider's layer id; the local provider's are `env`, `file`,
    /// `project-env` and `user-env`.
    pub source: String,
}

// The value is a secret, so it's never printed.
impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("value", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

/// What [`Credentials::resolve`] resolves to: the value, `None` while it
/// isn't configured, or the provider's failure.
pub type CredentialFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<ResolvedCredential>, String>> + Send + 'a>>;

/// The port. (dsh: `CredentialProvider`, its `resolve` only.)
pub trait Credentials: Send + Sync {
    fn resolve<'a>(&'a self, reference: &'a CredentialRef) -> CredentialFuture<'a>;
}

/// Looks one variable up.
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment, read at each resolve; an empty value is
/// unconfigured. (dsh: `credentials-local`'s `env` layer.)
#[derive(Clone)]
pub struct EnvCredentials {
    lookup: EnvLookup,
}

impl EnvCredentials {
    pub fn new() -> Self {
        Self::with_lookup(Arc::new(|name| std::env::var(name).ok()))
    }

    /// Resolves through `lookup` instead of the process environment.
    pub fn with_lookup(lookup: EnvLookup) -> Self {
        Self { lookup }
    }
}

impl Default for EnvCredentials {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for EnvCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvCredentials").finish_non_exhaustive()
    }
}

impl Credentials for EnvCredentials {
    fn resolve<'a>(&'a self, reference: &'a CredentialRef) -> CredentialFuture<'a> {
        Box::pin(async move {
            Ok((self.lookup)(reference.as_str())
                .filter(|value| !value.is_empty())
                .map(|value| ResolvedCredential {
                    value,
                    source: "env".into(),
                }))
        })
    }
}

/// Provides [`Credentials`]: the process environment, or the resolver it
/// was given.
pub struct CredentialsPlugin {
    credentials: Arc<dyn Credentials>,
}

impl CredentialsPlugin {
    /// The process environment.
    pub fn new() -> Self {
        Self::with(Arc::new(EnvCredentials::new()))
    }

    pub fn with(credentials: Arc<dyn Credentials>) -> Self {
        Self { credentials }
    }
}

impl Default for CredentialsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for CredentialsPlugin {
    fn name(&self) -> &str {
        "credentials"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<dyn Credentials>()]
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        ctx.provide::<dyn Credentials>(Arc::clone(&self.credentials));
        Ok(())
    }
}
