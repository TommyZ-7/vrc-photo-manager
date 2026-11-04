//! Tag catalog types shipped by the sidecar.

use serde::{Deserialize, Serialize};

/// The tag branch that `place_tags` runs against.
///
/// `other` is not a tag branch: the host maps it to [`TagBranch::Outdoor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagBranch {
    Indoor,
    Outdoor,
}

impl TagBranch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indoor => "indoor",
            Self::Outdoor => "outdoor",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "indoor" => Some(Self::Indoor),
            "outdoor" => Some(Self::Outdoor),
            _ => None,
        }
    }
}

/// The place branch returned by `place_class`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceBranch {
    Indoor,
    Outdoor,
    Other,
}

impl PlaceBranch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indoor => "indoor",
            Self::Outdoor => "outdoor",
            Self::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "indoor" => Some(Self::Indoor),
            "outdoor" => Some(Self::Outdoor),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

/// Model-wide defaults announced in the `hello` message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagDefaults {
    pub presence_threshold: f32,
    pub multiple_threshold: f32,
}

/// One tag in the sidecar catalog. The sidecar owns keys and thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagDefinition {
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub name_en: Option<String>,
    pub description: String,
    pub default_threshold: f32,
}

/// The tag catalog announced by the sidecar in `hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub version: String,
    pub outdoor: Vec<TagDefinition>,
    pub indoor: Vec<TagDefinition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_use_the_wire_strings() {
        assert_eq!(serde_json::to_value(TagBranch::Indoor).unwrap(), "indoor");
        assert_eq!(serde_json::to_value(TagBranch::Outdoor).unwrap(), "outdoor");
        assert_eq!(serde_json::to_value(PlaceBranch::Indoor).unwrap(), "indoor");
        assert_eq!(
            serde_json::to_value(PlaceBranch::Outdoor).unwrap(),
            "outdoor"
        );
        assert_eq!(serde_json::to_value(PlaceBranch::Other).unwrap(), "other");

        assert_eq!(TagBranch::parse("indoor"), Some(TagBranch::Indoor));
        assert_eq!(TagBranch::parse("outdoor"), Some(TagBranch::Outdoor));
        assert_eq!(TagBranch::parse("other"), None);
        assert_eq!(PlaceBranch::parse("indoor"), Some(PlaceBranch::Indoor));
        assert_eq!(PlaceBranch::parse("outdoor"), Some(PlaceBranch::Outdoor));
        assert_eq!(PlaceBranch::parse("other"), Some(PlaceBranch::Other));
        assert_eq!(PlaceBranch::parse("unknown"), None);
    }

    #[test]
    fn tag_definition_name_en_is_optional() {
        let json = r#"{"key":"japanese_room","name":"和室","description":"畳・障子のある和室","default_threshold":0.5}"#;
        let tag: TagDefinition = serde_json::from_str(json).unwrap();
        assert_eq!(tag.name_en, None);
        assert_eq!(tag.key, "japanese_room");
        assert_eq!(tag.default_threshold, 0.5);
    }

    #[test]
    fn catalog_requires_both_branches() {
        assert!(serde_json::from_str::<Catalog>(r#"{"version":"v","indoor":[]}"#).is_err());
        assert!(serde_json::from_str::<Catalog>(r#"{"version":"v","outdoor":[]}"#).is_err());

        let catalog: Catalog =
            serde_json::from_str(r#"{"version":"v","outdoor":[],"indoor":[]}"#).unwrap();
        assert!(catalog.outdoor.is_empty());
        assert!(catalog.indoor.is_empty());
    }
}
