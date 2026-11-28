//! Application tool policy: the allow and deny lists presets contribute for
//! application tools, per scope. (Karaka: `mcp-application/src/policy.ts`.)
//!
//! Unlike a global tool's restriction, an application tool needs an
//! explicit allow: an agent with no policy naming it never gets it.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use karaka_scope::{ScopeChain, ScopeKey};

/// The names one preset allows and denies. (Karaka: `ToolPolicy`.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplicationToolPolicy {
    /// Every list on an agent's chain must allow a name.
    pub allow: Option<Vec<String>>,
    /// Denied whatever any list on the chain allows.
    pub deny: Option<Vec<String>>,
}

/// Told after each change; its error refuses a new policy.
pub type PolicyListener = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// The policies of one runtime, and who is told of changes. (Karaka: the
/// module's `policies` and `listeners`, keyed by root.)
#[derive(Default)]
pub struct ApplicationPolicies {
    state: Mutex<State>,
    me: Weak<ApplicationPolicies>,
}

#[derive(Default)]
struct State {
    policies: Vec<(u64, ScopeKey, Arc<ApplicationToolPolicy>)>,
    listeners: Vec<(u64, PolicyListener)>,
    next: u64,
}

impl ApplicationPolicies {
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            state: Mutex::default(),
            me: me.clone(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether some list on `chain` allows `name` and none refuses it.
    /// (Karaka: `policyAllows`.)
    pub fn allows(&self, chain: &ScopeChain, name: &str) -> bool {
        let state = self.lock();
        let mut explicitly_allowed = false;
        for scope in chain.farthest_first() {
            for (_, key, policy) in &state.policies {
                if *key != scope {
                    continue;
                }
                if let Some(allow) = &policy.allow {
                    if !allow.iter().any(|allowed| allowed == name) {
                        return false;
                    }
                    explicitly_allowed = true;
                }
                if policy
                    .deny
                    .as_ref()
                    .is_some_and(|deny| deny.iter().any(|denied| denied == name))
                {
                    return false;
                }
            }
        }
        explicitly_allowed
    }

    /// Tells `listener` of every change until the watch is removed.
    /// (Karaka: `watchPolicy`.)
    pub fn watch(&self, listener: PolicyListener) -> PolicyWatch {
        let mut state = self.lock();
        state.next += 1;
        let id = state.next;
        state.listeners.push((id, listener));
        PolicyWatch {
            policies: self.me.clone(),
            id,
        }
    }

    /// Files `policy` in `scope`, and tells the listeners. (Karaka:
    /// `registerPolicy`.)
    ///
    /// # Errors
    ///
    /// No scope, in Karaka's words; or a listener's error, which takes the
    /// policy back out.
    pub fn register(
        &self,
        scope: Option<&ScopeKey>,
        policy: ApplicationToolPolicy,
    ) -> Result<PolicyRegistration, String> {
        let Some(scope) = scope else {
            return Err("Karaka tool policy must be mounted inside an Agent preset".into());
        };
        let id = {
            let mut state = self.lock();
            state.next += 1;
            let id = state.next;
            state.policies.push((id, scope.clone(), Arc::new(policy)));
            id
        };
        if let Err(error) = self.notify() {
            self.lock().policies.retain(|(known, _, _)| *known != id);
            return Err(error);
        }
        Ok(PolicyRegistration {
            policies: self.me.clone(),
            id,
        })
    }

    /// Calls each listener, outside the lock, so it can read the policies.
    fn notify(&self) -> Result<(), String> {
        let listeners: Vec<PolicyListener> = self
            .lock()
            .listeners
            .iter()
            .map(|(_, listener)| Arc::clone(listener))
            .collect();
        for listener in listeners {
            listener()?;
        }
        Ok(())
    }
}

impl fmt::Debug for ApplicationPolicies {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("ApplicationPolicies")
            .field("policies", &state.policies)
            .field("listeners", &state.listeners.len())
            .finish()
    }
}

/// One filed policy, until removed.
#[derive(Debug)]
#[must_use = "the policy applies until removed"]
pub struct PolicyRegistration {
    policies: Weak<ApplicationPolicies>,
    id: u64,
}

impl PolicyRegistration {
    /// Takes the policy out, and tells the listeners.
    ///
    /// # Errors
    ///
    /// A listener's; the policy is out regardless.
    pub fn remove(self) -> Result<(), String> {
        let Some(policies) = self.policies.upgrade() else {
            return Ok(());
        };
        policies
            .lock()
            .policies
            .retain(|(known, _, _)| *known != self.id);
        policies.notify()
    }
}

/// One listener, until removed.
#[derive(Debug)]
#[must_use = "the listener is told until removed"]
pub struct PolicyWatch {
    policies: Weak<ApplicationPolicies>,
    id: u64,
}

impl PolicyWatch {
    pub fn remove(self) {
        if let Some(policies) = self.policies.upgrade() {
            policies
                .lock()
                .listeners
                .retain(|(known, _)| *known != self.id);
        }
    }
}
