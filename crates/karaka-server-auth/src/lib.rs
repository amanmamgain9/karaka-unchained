//! Server-to-server authentication: which application server a request
//! comes from, and what to send that application's MCP endpoint. (Karaka's
//! own: `@karaka-ai/server-auth`, `server-auth/src/index.ts`.)
//!
//! [`ServerAuth`] is the seam; [`BearerServerAuth`] is Karaka's default,
//! one shared bearer per direction per application, each a credential
//! reference resolved at every use, so a rotated value applies to the next
//! request. Another scheme, such as Play This Life's delegated access,
//! is another [`ServerAuth`], given to [`ServerAuthPlugin::with`].
//!
//! Karaka passes an `AbortSignal` to both calls; here dropping the future
//! cancels it, so `server authentication cancelled` has nowhere to appear.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use karaka_credentials::{CredentialRef, Credentials};
use karaka_identity::ApplicationId;
use karaka_kernel::{BoxError, Plugin, PluginContext, PortId};

/// A verified application server. (Karaka: `AuthenticatedApplication`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedApplication {
    /// The application whose credential alone matched.
    pub application_id: ApplicationId,
}

/// What a [`ServerAuth`] call resolves to; the error is its message.
pub type AuthFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// The port, used for inbound chat and outbound tool traffic both.
/// (Karaka: `ServerAuth`.)
pub trait ServerAuth: Send + Sync {
    /// Verifies a whole inbound `Authorization` header: the application,
    /// or `None` when it doesn't verify. (Karaka: `authenticate`.)
    ///
    /// # Errors
    ///
    /// The credential provider failed.
    fn authenticate<'a>(
        &'a self,
        authorization: Option<&'a str>,
    ) -> AuthFuture<'a, Option<AuthenticatedApplication>>;

    /// The whole `Authorization` header for one application's MCP
    /// endpoint. (Karaka: `authorizeTools`.)
    ///
    /// # Errors
    ///
    /// The application is unknown, its credential isn't configured, or the
    /// provider failed.
    fn authorize_tools<'a>(&'a self, application: &'a ApplicationId) -> AuthFuture<'a, String>;
}

/// One application and its two credentials, each rotated on its own.
/// (Karaka: `ApplicationCredentialConfig`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationCredentialConfig {
    pub id: String,
    /// The reference inbound chat requests must bear.
    pub chat_credential: String,
    /// The reference sent to the application's MCP endpoint.
    pub tool_credential: String,
}

/// (Karaka: `BearerServerAuth`'s `Config`.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BearerConfig {
    pub applications: Vec<ApplicationCredentialConfig>,
}

struct Application {
    id: ApplicationId,
    chat: CredentialRef,
    tool: CredentialRef,
}

/// The shared-bearer provider. (Karaka: `BearerServerAuth`.)
pub struct BearerServerAuth {
    applications: Vec<Application>,
    credentials: Arc<dyn Credentials>,
}

impl BearerServerAuth {
    /// # Errors
    ///
    /// In Karaka's words: an empty or repeated application id, or a
    /// credential that isn't a reference.
    pub fn new(config: &BearerConfig, credentials: Arc<dyn Credentials>) -> Result<Self, String> {
        let mut applications: Vec<Application> = Vec::new();
        for application in &config.applications {
            if application.id.is_empty() {
                return Err("server-auth: application id must not be empty".into());
            }
            if applications
                .iter()
                .any(|known| known.id.as_str() == application.id)
            {
                return Err(format!(
                    "server-auth: duplicate application id \"{}\"",
                    application.id
                ));
            }
            applications.push(Application {
                id: ApplicationId::new(&application.id),
                chat: CredentialRef::new(&application.chat_credential)
                    .map_err(|error| error.to_string())?,
                tool: CredentialRef::new(&application.tool_credential)
                    .map_err(|error| error.to_string())?,
            });
        }
        Ok(Self {
            applications,
            credentials,
        })
    }
}

impl fmt::Debug for BearerServerAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BearerServerAuth")
            .field(
                "applications",
                &self
                    .applications
                    .iter()
                    .map(|application| application.id.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl ServerAuth for BearerServerAuth {
    /// Two applications sharing the supplied value verify neither.
    fn authenticate<'a>(
        &'a self,
        authorization: Option<&'a str>,
    ) -> AuthFuture<'a, Option<AuthenticatedApplication>> {
        Box::pin(async move {
            let Some(supplied) = authorization.and_then(bearer_token) else {
                return Ok(None);
            };
            let mut matched = None;
            for application in &self.applications {
                let configured = self.credentials.resolve(&application.chat).await?;
                if let Some(configured) = configured
                    && equal_secret(supplied, &configured.value)
                {
                    if matched.is_some() {
                        return Ok(None);
                    }
                    matched = Some(AuthenticatedApplication {
                        application_id: application.id.clone(),
                    });
                }
            }
            Ok(matched)
        })
    }

    fn authorize_tools<'a>(&'a self, application: &'a ApplicationId) -> AuthFuture<'a, String> {
        Box::pin(async move {
            let Some(found) = self
                .applications
                .iter()
                .find(|candidate| candidate.id == *application)
            else {
                return Err(format!(
                    "server-auth: unknown application \"{application}\""
                ));
            };
            match self.credentials.resolve(&found.tool).await? {
                Some(configured) => Ok(format!("Bearer {}", configured.value)),
                None => Err(format!(
                    "server-auth: tool credential for application \"{application}\" is not configured"
                )),
            }
        })
    }
}

/// The token of `Bearer <token>`, the scheme in any case, one space, and a
/// token with no whitespace. (Karaka: `bearerToken`, `/^Bearer ([^\s]+)$/iu`.)
pub fn bearer_token(authorization: &str) -> Option<&str> {
    let scheme = authorization.get(..7)?;
    if !scheme.eq_ignore_ascii_case("bearer ") {
        return None;
    }
    let token = &authorization[7..];
    // JavaScript's `\s` is Unicode's White_Space and the BOM.
    if token.is_empty() || token.chars().any(|c| c.is_whitespace() || c == '\u{feff}') {
        return None;
    }
    Some(token)
}

/// Compares in time that depends only on the lengths. (Karaka:
/// `equalSecret`, `timingSafeEqual` on equal lengths.)
fn equal_secret(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let difference = left
        .iter()
        .zip(right)
        .fold(0_u8, |difference, (l, r)| difference | (l ^ r));
    std::hint::black_box(difference) == 0
}

/// Provides [`ServerAuth`]: the shared-bearer provider over the runtime's
/// [`Credentials`], or a provider of the host's own. (Karaka: the
/// `server-auth` row.)
pub struct ServerAuthPlugin {
    source: Source,
}

enum Source {
    Bearer(BearerConfig),
    Given(Arc<dyn ServerAuth>),
}

impl ServerAuthPlugin {
    pub fn bearer(config: BearerConfig) -> Self {
        Self {
            source: Source::Bearer(config),
        }
    }

    pub fn with(auth: Arc<dyn ServerAuth>) -> Self {
        Self {
            source: Source::Given(auth),
        }
    }
}

impl Plugin for ServerAuthPlugin {
    fn name(&self) -> &str {
        "server-auth"
    }

    fn provides(&self) -> Vec<PortId> {
        vec![PortId::of::<dyn ServerAuth>()]
    }

    fn requires(&self) -> Vec<PortId> {
        match self.source {
            Source::Bearer(_) => vec![PortId::of::<dyn Credentials>()],
            Source::Given(_) => Vec::new(),
        }
    }

    async fn setup(&self, ctx: &mut PluginContext<'_>) -> Result<(), BoxError> {
        let auth: Arc<dyn ServerAuth> = match &self.source {
            Source::Bearer(config) => {
                Arc::new(BearerServerAuth::new(config, ctx.get::<dyn Credentials>())?)
            }
            Source::Given(auth) => Arc::clone(auth),
        };
        ctx.provide::<dyn ServerAuth>(auth);
        Ok(())
    }
}
