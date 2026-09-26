use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub uuid::Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }
            pub fn nil() -> Self {
                Self(uuid::Uuid::nil())
            }
            /// Short 8-char prefix for logs and UI.
            pub fn short(&self) -> String {
                self.0.simple().to_string()[..8].to_string()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0.hyphenated())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.short())
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                uuid::Uuid::parse_str(s).map(Self)
            }
        }
    };
}

id_type!(
    /// Identifies one terminal session (one PTY + one VT state) in the daemon.
    SessionId
);
id_type!(
    /// Identifies a workspace (a project root directory grouping sessions).
    WorkspaceId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_display_parse() {
        let id = SessionId::new();
        let parsed: SessionId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
        assert_eq!(id.short().len(), 8);
    }

    #[test]
    fn postcard_roundtrip() {
        let id = WorkspaceId::new();
        let bytes = postcard::to_stdvec(&id).unwrap();
        let back: WorkspaceId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, back);
    }
}
