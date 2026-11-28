//! The owner types and the in-memory owners.

use std::sync::Arc;

use karaka_identity::{
    ApplicationId, ApplicationOwner, IdentityError, IdentityErrorCode, InMemorySessionOwners,
    SessionOwners, SessionOwnersPlugin, TenantId, UserId, same_owner,
};
use karaka_kernel::Runtime;
use karaka_session::SessionId;

fn owner(application: &str, tenant: &str, user: &str) -> ApplicationOwner {
    ApplicationOwner {
        application_id: ApplicationId::new(application),
        tenant_id: TenantId::new(tenant),
        user_id: UserId::new(user),
    }
}

#[test]
fn same_owner_compares_all_three_ids() {
    let base = owner("app", "tenant", "user");
    assert!(same_owner(&base, &owner("app", "tenant", "user")));
    assert!(!same_owner(&base, &owner("other", "tenant", "user")));
    assert!(!same_owner(&base, &owner("app", "other", "user")));
    assert!(!same_owner(&base, &owner("app", "tenant", "other")));
}

#[test]
fn an_identity_error_is_its_message() {
    let error = IdentityError::new(
        IdentityErrorCode::Forbidden,
        "Chat is not owned by this caller",
    );
    assert_eq!(error.to_string(), "Chat is not owned by this caller");
    assert_eq!(error.code.as_str(), "forbidden");
    assert_eq!(IdentityErrorCode::Unavailable.as_str(), "unavailable");
    assert_eq!(IdentityError::NAME, "IdentityError");
}

#[tokio::test]
async fn the_host_sets_owners_both_lookups_read() {
    let owners = Arc::new(InMemorySessionOwners::new());
    let runtime = Runtime::start(vec![Box::new(SessionOwnersPlugin::with(Arc::clone(
        &owners,
    )))])
    .await
    .unwrap();
    let port = runtime.get::<dyn SessionOwners>();
    let chat = SessionId::new("chat");
    assert_eq!(port.owner_of(&chat).await, Ok(None));
    assert_eq!(port.owner_of_cached(&chat), Ok(None));
    owners.set(chat.clone(), owner("app", "tenant", "user"));
    assert_eq!(
        port.owner_of(&chat).await,
        Ok(Some(owner("app", "tenant", "user")))
    );
    assert_eq!(
        port.owner_of_cached(&chat),
        Ok(Some(owner("app", "tenant", "user")))
    );
    // The provided store is the one the plugin was given.
    runtime.get::<InMemorySessionOwners>().remove(&chat);
    assert_eq!(port.owner_of_cached(&chat), Ok(None));
}
