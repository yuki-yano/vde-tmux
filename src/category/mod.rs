pub mod identity;
pub mod model;
pub mod resolver;
pub mod store;

pub use identity::{RepoIdentity, resolve_repo_identity};
pub use model::{
    AUTOMATIC_LABEL, CATEGORY_STATE_SCHEMA_VERSION, CategoryIntent, CategoryName, CategorySource,
    CategoryState, EffectiveCategory, EffectiveCategoryModel, EffectiveRepoPlacement,
    MembershipTarget, RepoKey, UNCATEGORIZED, configured_category_names,
};
pub use resolver::{
    ResolvedSessionCategories, load_state_for_runner, load_state_for_server,
    resolve_project_category, resolve_project_category_from_server, resolve_session_categories,
    resolve_session_categories_from_runner, resolve_session_categories_from_server,
};
