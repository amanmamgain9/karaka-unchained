use std::any::{TypeId, type_name};
use std::fmt;
use std::hash::{Hash, Hasher};

/// Names a port: one capability that plugins provide and use.
///
/// A port is a trait, and its id is that trait's type, as in
/// `PortId::of::<dyn SessionStore>()`. Two ids are equal exactly when they name
/// the same type, so ports can't collide by sharing a name.
#[derive(Clone, Copy)]
pub struct PortId {
    type_id: TypeId,
    name: &'static str,
}

impl PortId {
    /// The id of the port for `T`, usually a trait object such as `dyn Clock`.
    pub fn of<T: ?Sized + 'static>() -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            name: type_name::<T>(),
        }
    }

    /// A readable name for messages, such as `dyn my_crate::Clock`.
    ///
    /// Only for people to read: Rust doesn't promise the exact text, so
    /// nothing should compare or parse it.
    pub fn name(&self) -> &'static str {
        self.name
    }
}

// Identity is the type alone; the name is for messages. These are written by
// hand because deriving them would compare and hash the name too.
impl PartialEq for PortId {
    fn eq(&self, other: &Self) -> bool {
        self.type_id == other.type_id
    }
}

impl Eq for PortId {}

impl Hash for PortId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.type_id.hash(state);
    }
}

impl fmt::Debug for PortId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "port `{}`", self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::PortId;

    trait Clock {}
    trait Greeter {}

    #[test]
    fn the_same_trait_gives_the_same_id() {
        assert_eq!(PortId::of::<dyn Clock>(), PortId::of::<dyn Clock>());
        assert_ne!(PortId::of::<dyn Clock>(), PortId::of::<dyn Greeter>());
    }

    #[test]
    fn the_name_says_which_trait_it_is() {
        assert!(PortId::of::<dyn Clock>().name().ends_with("Clock"));
    }
}
