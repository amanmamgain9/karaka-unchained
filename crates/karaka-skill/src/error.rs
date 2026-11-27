//! Why a registration or a read of the registry failed, in dsh's words.

use std::error::Error;
use std::fmt;

/// dsh's message for a lookup whose signal fired: the default reason of a
/// web `AbortController.abort()`. Karaka's signal carries no reason.
pub const ABORTED: &str = "This operation was aborted";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillError {
    /// The lookup's signal fired. (dsh: the signal's reason, thrown.)
    Aborted,
    /// A provider or a registration broke the contract, such as an invalid
    /// name or an empty description. The text is dsh's.
    Invalid(String),
    /// A provider failed to load a skill's body. (dsh: its rejection,
    /// thrown on.)
    Load(String),
    /// A provider name that is taken in its layer, or reserved.
    Provider(String),
    /// A config dsh refuses.
    Config(String),
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aborted => f.write_str(ABORTED),
            Self::Invalid(message)
            | Self::Load(message)
            | Self::Provider(message)
            | Self::Config(message) => f.write_str(message),
        }
    }
}

impl Error for SkillError {}
