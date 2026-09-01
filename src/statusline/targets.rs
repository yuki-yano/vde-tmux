use anyhow::{Result, anyhow};
use base64::Engine as _;
use sha2::{Digest, Sha256};

pub(super) const CATEGORY_RANGE_PREFIX: &str = "c:";
pub(super) const CURRENT_CATEGORY_RANGE_PREFIX: &str = "C:";
pub(crate) const ATTENTION_RANGE_PREFIX: &str = "p:";
// tmux stores `range=user|X` names in a 16-byte buffer, so X is limited to 15 bytes.
// A 9-byte digest becomes 12 base64url bytes and leaves one byte of headroom after the prefix.
const CATEGORY_TARGET_DIGEST_BYTES: usize = 9;
pub(super) const TMUX_USER_RANGE_NAME_MAX_BYTES: usize = 15;

pub(crate) fn category_target_key(category: &str) -> Result<String> {
    if category.len() > 256 {
        return Err(anyhow!("category key exceeds 256 UTF-8 bytes"));
    }
    let digest = Sha256::digest(category.as_bytes());
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(&digest[..CATEGORY_TARGET_DIGEST_BYTES]))
}

pub(crate) fn attention_target_key(pane: &crate::pane_state::PaneInstance) -> String {
    let mut hasher = Sha256::new();
    hasher.update(pane.pane_id.as_bytes());
    hasher.update([0]);
    hasher.update(pane.pane_pid.to_be_bytes());
    let digest = hasher.finalize();
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(&digest[..CATEGORY_TARGET_DIGEST_BYTES]);
    let target = format!("{ATTENTION_RANGE_PREFIX}{encoded}");
    debug_assert!(target.len() <= TMUX_USER_RANGE_NAME_MAX_BYTES);
    target
}

pub(crate) fn resolve_attention_target(
    entries: &[crate::daemon::protocol::v2::AttentionEntry],
    target: &str,
) -> Result<crate::pane_state::PaneInstance> {
    let entry = primary_attention_entry(entries)
        .ok_or_else(|| anyhow!("displayed attention target is no longer available"))?;
    if attention_target_key(&entry.pane_instance) != target {
        return Err(anyhow!(
            "displayed attention target is stale; wait for the status line to redraw"
        ));
    }
    Ok(entry.pane_instance.clone())
}

pub(super) fn validate_category_target(target: &str) -> Result<()> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(target)
        .map_err(|error| anyhow!("invalid category target encoding: {error}"))?;
    if bytes.len() != CATEGORY_TARGET_DIGEST_BYTES
        || base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes) != target
    {
        return Err(anyhow!("invalid category target encoding"));
    }
    Ok(())
}

pub(super) fn resolve_category_target(
    snapshot: &crate::daemon::protocol::v2::StatusSnapshot,
    target: &str,
) -> Result<String> {
    validate_category_target(target)?;
    let matches = snapshot
        .categories
        .iter()
        .filter(|category| !category.session_ids.is_empty())
        .filter(|category| {
            category_target_key(&category.category).is_ok_and(|candidate| candidate == target)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [category] => Ok(category.category.clone()),
        [] => Err(anyhow!(
            "displayed category target is no longer available; wait for the status line to redraw"
        )),
        _ => Err(anyhow!(
            "category target collision; rename one of the colliding categories"
        )),
    }
}

pub(super) fn primary_attention_entry(
    entries: &[crate::daemon::protocol::v2::AttentionEntry],
) -> Option<&crate::daemon::protocol::v2::AttentionEntry> {
    let mut primary = entries.first()?;
    for entry in &entries[1..] {
        if entry.elapsed_seconds > primary.elapsed_seconds {
            primary = entry;
        }
    }
    Some(primary)
}
