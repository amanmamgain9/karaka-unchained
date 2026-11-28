//! Observing one session, live or stored.

use std::sync::Arc;

use karaka_session::{SessionEvent, SessionId, SessionStore, UserMessage, UserSource};
use karaka_session_query::SessionQuery;

#[tokio::test]
async fn a_live_session_is_read_from_the_store_and_a_missing_one_is_not_found() {
    let store = Arc::new(SessionStore::new());
    let session = store.create(SessionId::new("chat")).unwrap();
    session
        .append(SessionEvent::user(UserMessage::text(
            "m",
            UserSource::user(),
            "hi",
        )))
        .unwrap();
    let query = SessionQuery::new(Arc::clone(&store), None);
    let observed = query
        .observe_session(&SessionId::new("chat"))
        .await
        .unwrap();
    assert!(observed.live);
    assert_eq!(observed.events.len(), 1);
    assert_eq!(&observed.header, session.header());
    let missing = query
        .observe_session(&SessionId::new("missing"))
        .await
        .unwrap_err();
    assert_eq!(missing.message, "session \"missing\" not found");
    assert_eq!(missing.code, "SESSION_QUERY_SESSION_NOT_FOUND");
}
