//! `SessionsPlugin` started in a Karaka runtime, and the store it provides.

use std::sync::Arc;

use karaka_kernel::Runtime;
use karaka_model::Message;
use karaka_session::{SessionEvent, SessionId, SessionStore, SessionsPlugin};

#[tokio::test]
async fn the_plugin_provides_an_empty_store() {
    let runtime = Runtime::start(vec![Box::new(SessionsPlugin)])
        .await
        .expect("the runtime starts");
    assert_eq!(runtime.plugins(), ["sessions"]);

    let store: Arc<SessionStore> = runtime.get::<SessionStore>();
    assert!(store.list().is_empty());

    let id = SessionId::new("a");
    store.create(id.clone()).expect("the id is free");
    store.append(&id, SessionEvent::user("hi")).unwrap();

    // Every read of the port reaches the same store.
    let again = runtime.get::<SessionStore>();
    assert_eq!(again.messages(&id), Ok(vec![Message::user("hi")]));
}
