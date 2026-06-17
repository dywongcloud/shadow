//! Strongly-typed identifiers for every level of the Hive hierarchy.
//!
//! Hive's topology is `Hive > Box > Cell`, with `Job`s flowing through it.
//! Using newtypes keeps us from ever passing a `BoxId` where a `CellId` is
//! expected.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            /// Generate a fresh random id, e.g. `cell-1a2b3c4d`.
            pub fn new() -> Self {
                let short = Uuid::new_v4().simple().to_string();
                $name(format!("{}-{}", $prefix, &short[..8]))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                $name(s.to_string())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                $name(s)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

id_type!(HiveId, "hive");
id_type!(BoxId, "box");
id_type!(CellId, "cell");
id_type!(JobId, "job");
