//! Branded string ids. Each is a distinct type so a `ThreadId` cannot be passed
//! where a `ProjectId` is expected.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($($(#[$meta:meta])* $name:ident),* $(,)?) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// A fresh random id in the same `uuid` form the web client uses.
            pub fn random() -> Self {
                Self(crate::ids::random_uuid())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    )*};
}

string_id!(SpaceId, ProjectId, ThreadId, MessageId, TurnId, CommandId, EventId,);

pub(crate) fn random_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialize_as_plain_strings() {
        let id = ThreadId::from("t-1");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"t-1\"");
        let back: ThreadId = serde_json::from_str("\"t-1\"").unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn random_ids_are_v4_uuids_and_distinct() {
        let a = CommandId::random();
        let b = CommandId::random();
        assert_ne!(a, b);
        let s = a.as_str();
        assert_eq!(s.len(), 36);
        assert_eq!(&s[14..15], "4");
        assert!(matches!(&s[19..20], "8" | "9" | "a" | "b"));
    }
}
