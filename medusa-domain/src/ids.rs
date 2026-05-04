use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl $name {
            pub const fn new(v: i64) -> Self {
                Self(v)
            }

            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_newtype!(RepoId);
id_newtype!(EvalId);
id_newtype!(JobId);
id_newtype!(BuilderId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_distinct_types() {
        let r = RepoId::new(1);
        let e = EvalId::new(1);
        let j = JobId::new(1);
        assert_eq!(r.get(), 1);
        assert_eq!(e.get(), 1);
        assert_eq!(j.get(), 1);
    }

    #[test]
    fn ids_display_as_integers() {
        assert_eq!(RepoId::new(42).to_string(), "42");
    }
}
