//! Typed ids so a CardId can never be passed where a DeckId belongs.

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub i64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl From<i64> for $name {
            fn from(v: i64) -> Self {
                Self(v)
            }
        }
    };
}

id_type!(DeckId);
id_type!(CardId);
id_type!(SessionId);
id_type!(MediaId);
id_type!(NoteId);

/// A user's row id — and, unlike the other ids, a capability.
///
/// Ordinary code cannot build one from an integer. A `UserId` in scope
/// is therefore one of two things: the authenticated user of the current
/// request (minted by the session and bearer extractors from a row the
/// store looked up), or a row the store returned. A handler holding a
/// number from a path segment or a request body cannot turn it into a
/// `UserId` and act as that user by mistake; every per-user query in the
/// store is scoped by one of these, so that mistake is not expressible.
///
/// The two constructors below are the whole escape hatch, and a test
/// pins where each may be called from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(i64);

impl UserId {
    /// The id of a row as the store read it. For the store's row mappers.
    pub fn from_db(raw: i64) -> Self {
        Self(raw)
    }

    /// An id that arrived from outside the database and that the caller
    /// has independently authorised: an admin acting on a user chosen in
    /// the admin UI, and test fixtures. The name is meant to stand out in
    /// review.
    pub fn assume_authorized(raw: i64) -> Self {
        Self(raw)
    }

    /// The integer, for SQL parameters, JSON and logs.
    pub fn raw(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
