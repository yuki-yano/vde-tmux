use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::sidebar::state::MoveDirection;

use super::identity::RepoIdentity;

pub const CATEGORY_STATE_SCHEMA_VERSION: u32 = 1;
pub const UNCATEGORIZED: &str = "Uncategorized";
pub const AUTOMATIC_LABEL: &str = "Automatic (config)";
const CATEGORY_NAME_MAX_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CategoryName(String);

impl CategoryName {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err("category name is empty".to_string());
        }
        if trimmed.len() > CATEGORY_NAME_MAX_BYTES {
            return Err(format!(
                "category name exceeds {CATEGORY_NAME_MAX_BYTES} UTF-8 bytes"
            ));
        }
        if trimmed.chars().any(char::is_control) {
            return Err("category name contains a control character".to_string());
        }
        if matches!(trimmed, UNCATEGORIZED | AUTOMATIC_LABEL) {
            return Err(format!("category name is reserved: {trimmed}"));
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn uncategorized() -> Self {
        Self(UNCATEGORIZED.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CategoryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoKey(String);

impl RepoKey {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let path = value
            .strip_prefix("git:")
            .or_else(|| value.strip_prefix("path:"))
            .ok_or_else(|| "repository key must start with git: or path:".to_string())?;
        if path.trim().is_empty() {
            return Err("repository key path is empty".to_string());
        }
        Ok(Self(value))
    }

    pub fn git(path: impl fmt::Display) -> Self {
        Self(format!("git:{path}"))
    }

    pub fn path(path: impl fmt::Display) -> Self {
        Self(format!("path:{path}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CategorySource {
    Configured,
    Dynamic,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum MembershipTarget {
    Category(CategoryName),
    Automatic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryState {
    pub schema_version: u32,
    pub revision: u64,
    #[serde(default)]
    pub dynamic_categories: BTreeSet<CategoryName>,
    #[serde(default)]
    pub category_order: Vec<CategoryName>,
    #[serde(default)]
    pub repo_overrides: BTreeMap<RepoKey, CategoryName>,
    #[serde(default)]
    pub repo_order: BTreeMap<CategoryName, Vec<RepoKey>>,
}

impl Default for CategoryState {
    fn default() -> Self {
        Self {
            schema_version: CATEGORY_STATE_SCHEMA_VERSION,
            revision: 0,
            dynamic_categories: BTreeSet::new(),
            category_order: Vec::new(),
            repo_overrides: BTreeMap::new(),
            repo_order: BTreeMap::new(),
        }
    }
}

impl CategoryState {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CATEGORY_STATE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported category state schema version: {}",
                self.schema_version
            ));
        }
        validate_unique(&self.category_order, "category order")?;
        for (category, repos) in &self.repo_order {
            validate_stored_category_name(category)?;
            validate_unique(repos, &format!("repository order for {category}"))?;
        }
        for category in &self.dynamic_categories {
            validate_stored_category_name(category)?;
        }
        for category in self.repo_overrides.values() {
            validate_stored_category_name(category)?;
        }
        Ok(())
    }

    pub fn apply_intent(
        &mut self,
        config: &Config,
        intent: &CategoryIntent,
        model: &EffectiveCategoryModel,
    ) -> Result<bool, String> {
        let configured = configured_category_names(config)?;
        let changed = match intent {
            CategoryIntent::CreateCategory { name } => {
                validate_mutable_name(name)?;
                if model.category(name).is_some() {
                    return Err(format!("category already exists: {name}"));
                }
                self.dynamic_categories.insert(name.clone());
                self.category_order.push(name.clone());
                true
            }
            CategoryIntent::RenameCategory {
                current,
                replacement,
            } => {
                validate_mutable_category(model, current)?;
                validate_mutable_name(replacement)?;
                if current == replacement {
                    false
                } else {
                    if model.category(replacement).is_some() {
                        return Err(format!("category already exists: {replacement}"));
                    }
                    replace_category_name(self, current, replacement);
                    true
                }
            }
            CategoryIntent::DeleteCategory {
                category,
                replacement,
            } => {
                validate_mutable_category(model, category)?;
                validate_delete_replacement(model, category, replacement)?;
                delete_category(self, config, model, category, replacement)?;
                true
            }
            CategoryIntent::AssignRepo { repo, category } => {
                if model.category(category).is_none() {
                    return Err(format!("unknown category: {category}"));
                }
                let previous = self.repo_overrides.insert(repo.clone(), category.clone());
                move_repo_to_category_end(self, repo, previous.as_ref(), category);
                previous.as_ref() != Some(category)
            }
            CategoryIntent::SetRepoAutomatic { repo } => {
                let previous = self.repo_overrides.remove(repo);
                if let Some(previous) = previous.as_ref()
                    && let Some(repos) = self.repo_order.get_mut(previous)
                {
                    repos.retain(|candidate| candidate != repo);
                }
                previous.is_some()
            }
            CategoryIntent::MoveCategory {
                category,
                neighbor,
                direction,
            } => {
                materialize_category_order(self, model);
                move_relative(&mut self.category_order, category, neighbor, *direction)?
            }
            CategoryIntent::MoveRepo {
                repo,
                neighbor,
                category,
                direction,
            } => {
                let effective = model
                    .placements
                    .get(repo)
                    .ok_or_else(|| format!("unknown repository: {repo}"))?;
                let neighbor_effective = model
                    .placements
                    .get(neighbor)
                    .ok_or_else(|| format!("unknown repository: {neighbor}"))?;
                if &effective.category != category || &neighbor_effective.category != category {
                    return Err("repository reorder cannot cross a category boundary".to_string());
                }
                let order = self.repo_order.entry(category.clone()).or_default();
                materialize_repo_order(order, model, category);
                move_relative(order, repo, neighbor, *direction)?
            }
        };
        if changed {
            self.revision = self
                .revision
                .checked_add(1)
                .ok_or_else(|| "category state revision overflow".to_string())?;
        }
        self.validate()?;
        for category in configured {
            validate_stored_category_name(&category)?;
        }
        Ok(changed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum CategoryIntent {
    CreateCategory {
        name: CategoryName,
    },
    RenameCategory {
        current: CategoryName,
        replacement: CategoryName,
    },
    DeleteCategory {
        category: CategoryName,
        replacement: MembershipTarget,
    },
    AssignRepo {
        repo: RepoKey,
        category: CategoryName,
    },
    SetRepoAutomatic {
        repo: RepoKey,
    },
    MoveCategory {
        category: CategoryName,
        neighbor: CategoryName,
        direction: MoveDirection,
    },
    MoveRepo {
        repo: RepoKey,
        neighbor: RepoKey,
        category: CategoryName,
        direction: MoveDirection,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveCategory {
    pub name: CategoryName,
    pub source: CategorySource,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveRepoPlacement {
    pub repo: RepoIdentity,
    pub category: CategoryName,
    pub explicit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveCategoryModel {
    pub revision: u64,
    pub categories: Vec<EffectiveCategory>,
    pub placements: BTreeMap<RepoKey, EffectiveRepoPlacement>,
}

impl EffectiveCategoryModel {
    pub fn build(
        config: &Config,
        state: &CategoryState,
        repos: impl IntoIterator<Item = RepoIdentity>,
    ) -> Result<Self, String> {
        state.validate()?;
        let configured = configured_category_names(config)?;
        let configured_set = configured.iter().cloned().collect::<BTreeSet<_>>();
        let mut names = configured_set.clone();
        names.extend(state.dynamic_categories.iter().cloned());
        names.extend(state.repo_overrides.values().cloned());
        names.insert(CategoryName::uncategorized());
        let ordered = effective_category_order(config, state, &names)?;
        let categories = ordered
            .into_iter()
            .map(|name| {
                let source = if name.as_str() == UNCATEGORIZED {
                    CategorySource::System
                } else if configured_set.contains(&name) {
                    CategorySource::Configured
                } else {
                    CategorySource::Dynamic
                };
                let display_name = config
                    .categories
                    .display_names
                    .get(name.as_str())
                    .cloned()
                    .unwrap_or_else(|| name.to_string());
                EffectiveCategory {
                    name,
                    source,
                    display_name,
                }
            })
            .collect::<Vec<_>>();
        let mut placements = BTreeMap::new();
        for repo in repos {
            let (category, explicit) = resolve_membership(config, state, &repo)?;
            placements.insert(
                repo.key.clone(),
                EffectiveRepoPlacement {
                    repo,
                    category,
                    explicit,
                },
            );
        }
        Ok(Self {
            revision: state.revision,
            categories,
            placements,
        })
    }

    pub fn category(&self, name: &CategoryName) -> Option<&EffectiveCategory> {
        self.categories
            .iter()
            .find(|category| &category.name == name)
    }

    pub fn ordered_repos(&self, state: &CategoryState, category: &CategoryName) -> Vec<RepoKey> {
        let mut repos = self
            .placements
            .iter()
            .filter(|(_, placement)| &placement.category == category)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let stored = state.repo_order.get(category);
        repos.sort_by(|left, right| {
            let left_order = stored.and_then(|order| order.iter().position(|key| key == left));
            let right_order = stored.and_then(|order| order.iter().position(|key| key == right));
            left_order
                .cmp(&right_order)
                .then_with(|| {
                    self.placements[left]
                        .repo
                        .display_name
                        .cmp(&self.placements[right].repo.display_name)
                })
                .then_with(|| left.cmp(right))
        });
        repos
    }
}

pub fn configured_category_names(config: &Config) -> Result<Vec<CategoryName>, String> {
    let mut names = BTreeSet::new();
    for name in config
        .categories
        .rules
        .iter()
        .map(|rule| rule.category.as_str())
        .chain(config.categories.default_category.as_deref())
        .chain(config.categories.display_names.keys().map(String::as_str))
        .chain(config.categories.order.keys().map(String::as_str))
    {
        names.insert(CategoryName::parse(name)?);
    }
    Ok(names.into_iter().collect())
}

fn effective_category_order(
    config: &Config,
    state: &CategoryState,
    names: &BTreeSet<CategoryName>,
) -> Result<Vec<CategoryName>, String> {
    let mut base = names.iter().cloned().collect::<Vec<_>>();
    base.sort_by(|left, right| {
        config
            .categories
            .order
            .get(left.as_str())
            .copied()
            .unwrap_or(i64::MAX)
            .cmp(
                &config
                    .categories
                    .order
                    .get(right.as_str())
                    .copied()
                    .unwrap_or(i64::MAX),
            )
            .then_with(|| left.cmp(right))
    });
    if state.category_order.is_empty() {
        return Ok(base);
    }
    let mut ordered = state
        .category_order
        .iter()
        .filter(|name| names.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    for name in base {
        if !ordered.contains(&name) {
            ordered.push(name);
        }
    }
    validate_unique(&ordered, "effective category order")?;
    Ok(ordered)
}

fn resolve_membership(
    config: &Config,
    state: &CategoryState,
    repo: &RepoIdentity,
) -> Result<(CategoryName, bool), String> {
    if let Some(category) = state.repo_overrides.get(&repo.key) {
        return Ok((category.clone(), true));
    }
    for rule in &config.categories.rules {
        if rule
            .path_patterns
            .iter()
            .any(|pattern| matches_path_pattern(pattern, &repo.rule_path))
        {
            return Ok((CategoryName::parse(&rule.category)?, false));
        }
    }
    if let Some(default) = config
        .categories
        .default_category
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok((CategoryName::parse(default)?, false));
    }
    Ok((CategoryName::uncategorized(), false))
}

fn matches_path_pattern(pattern: &str, path: &str) -> bool {
    if wildcard_match(pattern, path) {
        return true;
    }
    path.match_indices('/').any(|(index, _)| {
        let suffix = &path[index + 1..];
        wildcard_match(pattern, suffix)
    })
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let (mut pattern_index, mut text_index) = (0, 0);
    let mut star_index = None;
    let mut match_index = 0;
    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == text[text_index] || pattern[pattern_index] == b'?')
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            match_index = text_index;
            pattern_index += 1;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            match_index += 1;
            text_index = match_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn validate_unique<T>(values: &[T], label: &str) -> Result<(), String>
where
    T: Ord + Clone,
{
    let unique = values.iter().cloned().collect::<BTreeSet<_>>();
    if unique.len() != values.len() {
        return Err(format!("{label} contains duplicate entries"));
    }
    Ok(())
}

fn validate_stored_category_name(name: &CategoryName) -> Result<(), String> {
    if name.as_str() == UNCATEGORIZED {
        return Ok(());
    }
    CategoryName::parse(name.as_str()).map(|_| ())
}

fn validate_mutable_name(name: &CategoryName) -> Result<(), String> {
    CategoryName::parse(name.as_str()).map(|_| ())
}

fn validate_mutable_category(
    model: &EffectiveCategoryModel,
    category: &CategoryName,
) -> Result<(), String> {
    let category = model
        .category(category)
        .ok_or_else(|| format!("unknown category: {category}"))?;
    match category.source {
        CategorySource::Dynamic => Ok(()),
        CategorySource::Configured => Err(format!(
            "configured category is read-only: {}",
            category.name
        )),
        CategorySource::System => Err(format!("system category is read-only: {}", category.name)),
    }
}

fn validate_delete_replacement(
    model: &EffectiveCategoryModel,
    category: &CategoryName,
    replacement: &MembershipTarget,
) -> Result<(), String> {
    if let MembershipTarget::Category(replacement) = replacement {
        if replacement == category {
            return Err("category cannot be deleted into itself".to_string());
        }
        if model.category(replacement).is_none() {
            return Err(format!("unknown replacement category: {replacement}"));
        }
    }
    Ok(())
}

fn replace_category_name(
    state: &mut CategoryState,
    current: &CategoryName,
    replacement: &CategoryName,
) {
    state.dynamic_categories.remove(current);
    state.dynamic_categories.insert(replacement.clone());
    for category in &mut state.category_order {
        if category == current {
            *category = replacement.clone();
        }
    }
    for category in state.repo_overrides.values_mut() {
        if category == current {
            *category = replacement.clone();
        }
    }
    if let Some(order) = state.repo_order.remove(current) {
        state.repo_order.insert(replacement.clone(), order);
    }
}

fn delete_category(
    state: &mut CategoryState,
    config: &Config,
    model: &EffectiveCategoryModel,
    category: &CategoryName,
    replacement: &MembershipTarget,
) -> Result<(), String> {
    let affected = state
        .repo_overrides
        .iter()
        .filter(|(_, assigned)| *assigned == category)
        .map(|(repo, _)| repo.clone())
        .collect::<Vec<_>>();
    state.dynamic_categories.remove(category);
    state
        .category_order
        .retain(|candidate| candidate != category);
    state.repo_order.remove(category);
    for repo in affected {
        match replacement {
            MembershipTarget::Category(replacement) => {
                state
                    .repo_overrides
                    .insert(repo.clone(), replacement.clone());
                move_repo_to_category_end(state, &repo, None, replacement);
            }
            MembershipTarget::Automatic => {
                state.repo_overrides.remove(&repo);
                if let Some(placement) = model.placements.get(&repo) {
                    let automatic = resolve_membership(config, state, &placement.repo)?.0;
                    move_repo_to_category_end(state, &repo, None, &automatic);
                }
            }
        }
    }
    Ok(())
}

fn move_repo_to_category_end(
    state: &mut CategoryState,
    repo: &RepoKey,
    previous: Option<&CategoryName>,
    category: &CategoryName,
) {
    for (candidate, repos) in &mut state.repo_order {
        if previous.is_none_or(|previous| candidate == previous) || candidate == category {
            repos.retain(|value| value != repo);
        }
    }
    state
        .repo_order
        .entry(category.clone())
        .or_default()
        .push(repo.clone());
}

fn materialize_category_order(state: &mut CategoryState, model: &EffectiveCategoryModel) {
    let current = state.category_order.clone();
    state.category_order = model
        .categories
        .iter()
        .map(|category| category.name.clone())
        .collect();
    for category in current {
        if !state.category_order.contains(&category) {
            state.category_order.push(category);
        }
    }
}

fn materialize_repo_order(
    order: &mut Vec<RepoKey>,
    model: &EffectiveCategoryModel,
    category: &CategoryName,
) {
    for repo in model
        .placements
        .iter()
        .filter(|(_, placement)| &placement.category == category)
        .map(|(repo, _)| repo)
    {
        if !order.contains(repo) {
            order.push(repo.clone());
        }
    }
}

fn move_relative<T>(
    values: &mut [T],
    value: &T,
    neighbor: &T,
    direction: MoveDirection,
) -> Result<bool, String>
where
    T: Eq,
{
    if value == neighbor {
        return Err("value and neighbor must differ".to_string());
    }
    let index = values
        .iter()
        .position(|candidate| candidate == value)
        .ok_or_else(|| "value is not present in order".to_string())?;
    let neighbor_index = values
        .iter()
        .position(|candidate| candidate == neighbor)
        .ok_or_else(|| "neighbor is not present in order".to_string())?;
    let expected = match direction {
        MoveDirection::Up => index.checked_sub(1),
        MoveDirection::Down => index.checked_add(1).filter(|next| *next < values.len()),
    };
    if expected != Some(neighbor_index) {
        return Err("value and neighbor are not adjacent in the requested direction".to_string());
    }
    values.swap(index, neighbor_index);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryRule;

    fn category(value: &str) -> CategoryName {
        if value == UNCATEGORIZED {
            CategoryName::uncategorized()
        } else {
            CategoryName::parse(value).unwrap()
        }
    }

    fn repo(key: &str, path: &str) -> RepoIdentity {
        RepoIdentity {
            key: RepoKey::path(key),
            rule_path: path.to_string(),
            display_name: key.to_string(),
        }
    }

    #[test]
    fn explicit_membership_overrides_rules_and_automatic_restores_them() {
        let mut config = Config::default();
        config.categories.rules.push(CategoryRule {
            category: "work".to_string(),
            path_patterns: vec!["github.com/acme/*".to_string()],
        });
        let identity = repo("app", "/src/github.com/acme/app");
        let mut state = CategoryState::default();
        state.dynamic_categories.insert(category("focus"));
        state
            .repo_overrides
            .insert(identity.key.clone(), category("focus"));

        let model = EffectiveCategoryModel::build(&config, &state, [identity.clone()]).unwrap();
        assert_eq!(model.placements[&identity.key].category, category("focus"));
        assert!(model.placements[&identity.key].explicit);

        let mut candidate = state.clone();
        candidate
            .apply_intent(
                &config,
                &CategoryIntent::SetRepoAutomatic {
                    repo: identity.key.clone(),
                },
                &model,
            )
            .unwrap();
        let automatic = EffectiveCategoryModel::build(&config, &candidate, [identity]).unwrap();
        assert_eq!(
            automatic.placements.values().next().unwrap().category,
            category("work")
        );
        assert!(!automatic.placements.values().next().unwrap().explicit);
    }

    #[test]
    fn configured_removed_but_referenced_category_becomes_dynamic() {
        let identity = repo("app", "/src/app");
        let mut state = CategoryState::default();
        state
            .repo_overrides
            .insert(identity.key.clone(), category("former-config"));

        let model = EffectiveCategoryModel::build(&Config::default(), &state, [identity]).unwrap();

        assert_eq!(
            model.category(&category("former-config")).unwrap().source,
            CategorySource::Dynamic
        );
    }

    #[test]
    fn explicit_uncategorized_does_not_follow_new_rule() {
        let identity = repo("app", "/src/app");
        let mut state = CategoryState::default();
        state
            .repo_overrides
            .insert(identity.key.clone(), CategoryName::uncategorized());
        let mut config = Config::default();
        config.categories.rules.push(CategoryRule {
            category: "work".to_string(),
            path_patterns: vec!["/src/*".to_string()],
        });

        let model = EffectiveCategoryModel::build(&config, &state, [identity]).unwrap();

        assert_eq!(
            model.placements.values().next().unwrap().category,
            CategoryName::uncategorized()
        );
        assert!(model.placements.values().next().unwrap().explicit);
    }

    #[test]
    fn category_and_repo_reorder_require_adjacent_same_category_neighbors() {
        let config = Config::default();
        let a = repo("a", "/a");
        let b = repo("b", "/b");
        let mut state = CategoryState::default();
        state
            .dynamic_categories
            .extend([category("one"), category("two")]);
        state.category_order = vec![category("one"), category("two")];
        state.repo_overrides.insert(a.key.clone(), category("one"));
        state.repo_overrides.insert(b.key.clone(), category("one"));
        let model = EffectiveCategoryModel::build(&config, &state, [a.clone(), b.clone()]).unwrap();

        state
            .apply_intent(
                &config,
                &CategoryIntent::MoveCategory {
                    category: category("two"),
                    neighbor: category("one"),
                    direction: MoveDirection::Up,
                },
                &model,
            )
            .unwrap();
        assert_eq!(
            &state.category_order[..2],
            &[category("two"), category("one")]
        );

        let model = EffectiveCategoryModel::build(&config, &state, [a.clone(), b.clone()]).unwrap();
        state
            .apply_intent(
                &config,
                &CategoryIntent::MoveRepo {
                    repo: b.key.clone(),
                    neighbor: a.key.clone(),
                    category: category("one"),
                    direction: MoveDirection::Up,
                },
                &model,
            )
            .unwrap();
        assert_eq!(state.repo_order[&category("one")], vec![b.key, a.key]);
    }
}
