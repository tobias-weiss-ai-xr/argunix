use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    Github,
    Gitlab,
    Forgejo,
}

impl ForgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ForgeKind::Github => "github",
            ForgeKind::Gitlab => "gitlab",
            ForgeKind::Forgejo => "forgejo",
        }
    }
}

impl fmt::Display for ForgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_serde_names() {
        let k: ForgeKind = serde_json::from_str("\"github\"").unwrap();
        assert_eq!(k, ForgeKind::Github);
        let k: ForgeKind = serde_json::from_str("\"gitlab\"").unwrap();
        assert_eq!(k, ForgeKind::Gitlab);
        let k: ForgeKind = serde_json::from_str("\"forgejo\"").unwrap();
        assert_eq!(k, ForgeKind::Forgejo);
    }

    #[test]
    fn unknown_rejected() {
        assert!(serde_json::from_str::<ForgeKind>("\"gerrit\"").is_err());
    }
}
