// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use serde::{Deserialize, Serialize};

pub type TagId = u64;

const MAX_TAG_NAME_CHARS: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TagOrigin {
    #[default]
    Manual,
    Generated {
        provider: String,
        key: String,
    },
}

impl TagOrigin {
    pub fn is_manual(&self) -> bool {
        matches!(self, Self::Manual)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TagDefinition {
    pub id: TagId,
    pub name: String,
    pub origin: TagOrigin,
}

impl Default for TagDefinition {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            origin: TagOrigin::Manual,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TagCatalog {
    pub next_id: TagId,
    pub tags: Vec<TagDefinition>,
}

impl Default for TagCatalog {
    fn default() -> Self {
        Self {
            next_id: 1,
            tags: Vec::new(),
        }
    }
}

impl TagCatalog {
    pub fn create_manual(&mut self, name: &str) -> Result<TagId, String> {
        let name = validate_tag_name(name)?;
        self.ensure_unique_name(&name, None)?;

        let mut id = self.next_id.max(1);
        while self.tags.iter().any(|tag| tag.id == id) {
            id = id
                .checked_add(1)
                .ok_or_else(|| "Tag ID space is exhausted".to_string())?;
        }
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| "Tag ID space is exhausted".to_string())?;
        self.tags.push(TagDefinition {
            id,
            name,
            origin: TagOrigin::Manual,
        });
        Ok(id)
    }

    pub fn rename_manual(&mut self, id: TagId, new_name: &str) -> Result<(), String> {
        let new_name = validate_tag_name(new_name)?;
        self.ensure_unique_name(&new_name, Some(id))?;
        let tag = self
            .tags
            .iter_mut()
            .find(|tag| tag.id == id)
            .ok_or_else(|| format!("Tag '{}' was not found", id))?;
        if !tag.origin.is_manual() {
            return Err(format!("Generated tag '{}' cannot be renamed", tag.name));
        }
        tag.name = new_name;
        Ok(())
    }

    pub fn delete_manual(&mut self, id: TagId) -> Result<TagDefinition, String> {
        let index = self
            .tags
            .iter()
            .position(|tag| tag.id == id)
            .ok_or_else(|| format!("Tag '{}' was not found", id))?;
        if !self.tags[index].origin.is_manual() {
            return Err(format!(
                "Generated tag '{}' cannot be deleted",
                self.tags[index].name
            ));
        }
        Ok(self.tags.remove(index))
    }

    pub fn get(&self, id: TagId) -> Option<&TagDefinition> {
        self.tags.iter().find(|tag| tag.id == id)
    }

    pub fn resolve(&self, reference: &str) -> Result<&TagDefinition, String> {
        let reference = reference.trim();
        if let Ok(id) = reference.parse::<TagId>() {
            if let Some(tag) = self.get(id) {
                return Ok(tag);
            }
        }
        self.tags
            .iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(reference))
            .ok_or_else(|| format!("Tag '{}' was not found", reference))
    }

    pub fn repair(&mut self) {
        self.tags
            .retain(|tag| tag.id != 0 && validate_tag_name(&tag.name).is_ok());
        self.tags.sort_by_key(|tag| tag.id);
        self.tags.dedup_by_key(|tag| tag.id);

        let mut seen_names = Vec::<String>::new();
        self.tags.retain(|tag| {
            let folded = tag.name.to_ascii_lowercase();
            if seen_names.contains(&folded) {
                false
            } else {
                seen_names.push(folded);
                true
            }
        });

        let minimum_next = self
            .tags
            .iter()
            .map(|tag| tag.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        self.next_id = self.next_id.max(minimum_next);
    }

    fn ensure_unique_name(&self, name: &str, except_id: Option<TagId>) -> Result<(), String> {
        if self
            .tags
            .iter()
            .any(|tag| Some(tag.id) != except_id && tag.name.eq_ignore_ascii_case(name))
        {
            return Err(format!("A tag named '{}' already exists", name));
        }
        Ok(())
    }
}

pub fn validate_tag_name(name: &str) -> Result<String, String> {
    let normalized = name.trim();
    if normalized.is_empty() {
        return Err("Tag name cannot be empty".to_string());
    }
    if normalized.chars().count() > MAX_TAG_NAME_CHARS {
        return Err(format!(
            "Tag name cannot be longer than {} characters",
            MAX_TAG_NAME_CHARS
        ));
    }
    if normalized.chars().any(char::is_control) {
        return Err("Tag name cannot contain control characters".to_string());
    }
    if normalized.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(
            "Tag name cannot be numeric because numbers are reserved for tag IDs".to_string(),
        );
    }
    Ok(normalized.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_tag_lifecycle_keeps_stable_id() {
        let mut catalog = TagCatalog::default();
        let id = catalog.create_manual("Archive").expect("create tag");
        catalog.rename_manual(id, "Research").expect("rename tag");
        assert_eq!(
            catalog.get(id).map(|tag| tag.name.as_str()),
            Some("Research")
        );
        assert_eq!(catalog.delete_manual(id).expect("delete tag").id, id);
    }

    #[test]
    fn names_are_trimmed_and_case_insensitively_unique() {
        let mut catalog = TagCatalog::default();
        catalog.create_manual("  Archive  ").expect("create tag");
        let error = catalog
            .create_manual("archive")
            .expect_err("duplicate should fail");
        assert!(error.contains("already exists"));
    }

    #[test]
    fn numeric_names_are_rejected_for_create_and_rename() {
        let mut catalog = TagCatalog::default();
        let error = catalog
            .create_manual("123")
            .expect_err("numeric name should fail");
        assert!(error.contains("reserved for tag IDs"));

        let id = catalog.create_manual("Archive").expect("create tag");
        let error = catalog
            .rename_manual(id, "42")
            .expect_err("numeric rename should fail");
        assert!(error.contains("reserved for tag IDs"));
        assert_eq!(
            catalog.get(id).map(|tag| tag.name.as_str()),
            Some("Archive")
        );
    }

    #[test]
    fn repair_uses_the_same_ascii_case_folding_as_name_resolution() {
        let mut catalog = TagCatalog {
            next_id: 1,
            tags: vec![
                TagDefinition {
                    id: 1,
                    name: "Äther".into(),
                    origin: TagOrigin::Manual,
                },
                TagDefinition {
                    id: 2,
                    name: "äther".into(),
                    origin: TagOrigin::Manual,
                },
                TagDefinition {
                    id: 3,
                    name: "ARCHIVE".into(),
                    origin: TagOrigin::Manual,
                },
                TagDefinition {
                    id: 4,
                    name: "archive".into(),
                    origin: TagOrigin::Manual,
                },
            ],
        };

        catalog.repair();

        assert_eq!(
            catalog
                .tags
                .iter()
                .map(|tag| tag.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Äther", "äther", "ARCHIVE"]
        );
    }

    #[test]
    fn repair_drops_invalid_and_duplicate_entries_and_advances_id() {
        let mut catalog = TagCatalog {
            next_id: 1,
            tags: vec![
                TagDefinition {
                    id: 2,
                    name: "Archive".into(),
                    origin: TagOrigin::Manual,
                },
                TagDefinition {
                    id: 2,
                    name: "Duplicate ID".into(),
                    origin: TagOrigin::Manual,
                },
                TagDefinition {
                    id: 3,
                    name: "archive".into(),
                    origin: TagOrigin::Manual,
                },
                TagDefinition::default(),
            ],
        };
        catalog.repair();
        assert_eq!(catalog.tags.len(), 1);
        assert_eq!(catalog.next_id, 3);
    }
}
