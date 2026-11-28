//! The pre-step hook's scoping: a listener hears the agents whose chain
//! holds its scope, in the order listeners were added. (dsh: the
//! `agent/pre-step` waterfall dispatched through `scopeTarget`.)

use std::sync::Arc;

use karaka_agent::{
    Agent, FrameListener, FrameListeners, FrameSubscription, InboxError, PreStep, PreStepDecision,
    PreStepFuture, PreStepHooks, PreStepListener, PreStepNext, WhenIdle, cancellation,
};
use karaka_scope::{PresetId, ScopeChain, ScopeKey};
use karaka_session::{CancelCause, SessionId, UserMessage, UserSource};

struct Stub {
    id: SessionId,
    chain: ScopeChain,
    frames: FrameListeners,
}

impl Agent for Stub {
    fn id(&self) -> &SessionId {
        &self.id
    }

    fn followup(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn steer(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn inject(&self, _: UserMessage) -> Result<(), InboxError> {
        Ok(())
    }

    fn cancel(&self, _: CancelCause) {}

    fn when_idle(&self) -> WhenIdle<'_> {
        Box::pin(async {})
    }

    fn subscribe(&self, listener: FrameListener) -> FrameSubscription {
        self.frames.subscribe(listener)
    }

    fn scope(&self) -> ScopeChain {
        self.chain.clone()
    }
}

fn agent(preset: Option<&str>, id: &str) -> Arc<dyn Agent> {
    Arc::new(Stub {
        id: SessionId::new(id),
        chain: ScopeChain::new(preset.map(PresetId::new), Some(SessionId::new(id))),
        frames: FrameListeners::new(),
    })
}

/// Adds a message named after itself, after what the rest of the chain
/// enters with.
struct Adds(&'static str);

impl PreStepListener for Adds {
    fn decide<'a>(&'a self, _: &'a PreStep, next: PreStepNext<'a>) -> PreStepFuture<'a> {
        Box::pin(async move {
            let PreStepDecision::Enter { mut messages } = next.run().await? else {
                return Ok(PreStepDecision::Reject);
            };
            messages.push(UserMessage::text(self.0, UserSource::user(), self.0));
            Ok(PreStepDecision::Enter { messages })
        })
    }
}

/// What enters a step of `agent`, by message id.
async fn entered(hooks: &PreStepHooks, agent: &Arc<dyn Agent>) -> Vec<String> {
    let (_switch, cancel) = cancellation();
    let step = PreStep {
        agent: Arc::clone(agent),
        messages: Vec::new(),
        turn: 1,
        step: 1,
        cancel,
    };
    match hooks.run(&step).await.unwrap() {
        PreStepDecision::Enter { messages } => {
            messages.into_iter().map(|message| message.id).collect()
        }
        PreStepDecision::Reject => panic!("rejected"),
    }
}

#[tokio::test]
async fn a_listener_hears_the_agents_whose_chain_holds_its_scope() {
    let hooks = PreStepHooks::new();
    hooks.register(None, Arc::new(Adds("global")));
    hooks.register(
        Some(&ScopeKey::from(PresetId::new("coach"))),
        Arc::new(Adds("coach")),
    );
    hooks.register(
        Some(&ScopeKey::from(SessionId::new("s1"))),
        Arc::new(Adds("s1")),
    );
    hooks.register(None, Arc::new(Adds("global-later")));

    // Each listener runs the rest of the chain first, so the first one
    // added adds last.
    let coached = agent(Some("coach"), "s1");
    assert_eq!(
        entered(&hooks, &coached).await,
        ["global-later", "s1", "coach", "global"]
    );
    let sibling = agent(Some("coach"), "s2");
    assert_eq!(
        entered(&hooks, &sibling).await,
        ["global-later", "coach", "global"]
    );
    // Another preset, a session named like the preset, a session alone.
    for other in [
        agent(Some("other"), "s3"),
        agent(None, "coach"),
        agent(None, "s4"),
    ] {
        assert_eq!(entered(&hooks, &other).await, ["global-later", "global"]);
    }

    let scoped_only = PreStepHooks::new();
    scoped_only.register(
        Some(&ScopeKey::from(PresetId::new("coach"))),
        Arc::new(Adds("coach")),
    );
    assert!(!scoped_only.is_empty_for(&coached.scope()));
    assert!(scoped_only.is_empty_for(&ScopeChain::session(SessionId::new("s1"))));
    assert!(scoped_only.is_empty_for(&ScopeChain::global()));
}
