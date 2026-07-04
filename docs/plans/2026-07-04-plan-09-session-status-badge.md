# Plan 09: statusline sessions への 4 色 agent バッジ

> 2026-07-04 作成。herdr(https://herdr.dev/)の agent 状態表示を参考に、
> `vt statusline-sessions` が描画する **session リストの各 session** に
> session 内 agent の集約状態バッジを載せる。

## 0. 決定事項(ユーザー確定済み)

| 論点 | 決定 |
|---|---|
| 表示単位 | **session 単位**。session 内の全 agent pane を集約し、statusline の sessions リストの各 session ラベルにバッジを付ける |
| 状態マッピング | 4 色に集約。🔴 Blocked ← Error/Permission/Waiting、🟡 Working ← Running/Background、🔵 Done ← Idle(未読)、🟢 Idle ← Idle(既読) |
| グリフ | config で切替可能。デフォルトは絵文字(🔴🟡🔵🟢) |
| スペーシング | 絵文字は表示幅が広いため、**グリフ直後に挟む `suffix` を config に持たせ、デフォルト半角スペース 1 個**。バッジ値自体に含めるのでフォーマット側での調整は不要 |
| 既読条件 | pane 単位で判定: その pane の window がアタッチ中セッションのカレント window になったら既読(= 完了を実際に見た) |
| daemon 不在時 | graceful shutdown 時に全 session バッジを削除して stale 表示を防ぐ。クラッシュ時に残るのは既知の限界として許容。daemon 停止中はバッジ非表示(statusline 側での代替計算はしない) |

## 1. 設計サマリ

- 新しい session option **`@vde_session_status`** に「グリフ + suffix」の文字列を書く。
  **writer は daemon のみ**(per-key single writer 原則)。
- daemon の中央ループが `PanesUpdated` のたびに **session ごと**の rollup を計算し、
  **変化した session だけ** set/unset する(tmux コマンドの無駄撃ちをしない)。
- `vt statusline-sessions` は既存の `list-sessions` 1 コールのフォーマットに
  `#{@vde_session_status}` を足して読み、各 session セグメントのラベル先頭に
  バッジを前置して描画する(option 未設定なら空文字列 = 従来表示のまま)。
  **`.tmux.conf` の status-left は既に statusline-sessions を呼んでいるため、
  dotfiles の変更は不要**。
- 未読(Done)の判定は daemon 内のインメモリ状態で **pane 単位**に行い、
  session バッジはそれを集約する。
  - pane の RollupLevel が「非 Idle → Idle」に遷移した時点で未読フラグを立てる。
  - pane の window が「アタッチ中セッションのカレント window」である間は常に既読化する
    (見ながら完了した場合は青にならない)。
  - daemon 起動直後に初めて観測した Idle pane は未読にしない(起動時に青一色になるのを防ぐ)。
  - daemon 再起動で未読情報は消える(使い捨てキャッシュ思想。永続化はしない = non-goal)。
- 既読判定に必要な「カレント window か」「アタッチされているか」は、poll worker の
  `list-panes -a` フォーマットに `#{window_active}` / `#{session_attached}` を足して取得する。
  tmux hook の追加は不要。
- session 集約は pane ごとの 4 状態の **min**(注意度の高い順 Blocked < Working < Done < Idle)。
  sidebar pane(`@vde_sidebar`)と agent 無し pane は集約対象外。agent pane が
  1 つも無い session はバッジ無し(option を消す)。
- runtime は effect(`SetSessionBadge` / `ClearSessionBadge`)を返す純粋ロジックにし、
  実際の tmux 書き込みは `WorkerIo` 経由で server ループが行う(既存の
  `JumpPane`/`SaveState` と同じパターン)。effect 実行エラーは eprintln のみで daemon は死なない。

### non-goals

- daemon クラッシュ時の stale バッジ検出/自動掃除(graceful shutdown のみ対応)
- 未読情報の state.json 永続化(daemon 再起動後は全て既読からスタート)
- daemon 停止中の statusline 側での rollup 代替計算(未読が区別できず、
  statusline 更新のたびに `list-panes -a` が走るコストも見合わない)
- window 単位バッジ(`window-status-format` 側)。将来やる場合は本計画の
  集約キーを window_id に変えるだけで流用できる
- 旧 `@pane_*` キーへの書き込み

## 2. 共通ルール

- 各タスクは RED(失敗するテスト)→ GREEN(実装)→ 品質ゲート → コミット、の順で進める。
- コミット前に必ず `cargo fmt` を実行し、整形結果を採用する。
- 品質ゲート: `cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
- コミットメッセージは日本語。複数行はヒアドキュメント形式(`git commit -m "$(cat <<'EOF' ... EOF)"`)。
- 検証は scratch tmux(`tmux -L <name> -f /dev/null`)のみ。本番 tmux・本番 daemon に触れない。
- `cargo install` はしない。dotfiles はコミットせず、変更が必要な場合のみ diff 提示。

---

## Task 1: config に session_badge を追加する

### Step 1: RED — テストを書く

`src/config/mod.rs` のテストモジュールに追加:

```rust
#[test]
fn session_badge_defaults_to_emoji_glyphs_with_space_suffix() {
    let config = SessionBadgeConfig::default();
    assert!(config.enabled);
    assert_eq!(config.suffix, " ");
    assert_eq!(config.glyphs.blocked, "🔴");
    assert_eq!(config.glyphs.working, "🟡");
    assert_eq!(config.glyphs.done, "🔵");
    assert_eq!(config.glyphs.idle, "🟢");
}
```

`src/config/load.rs` のテストモジュールに追加(部分指定で残りが default に落ちること):

```rust
#[test]
fn parse_config_accepts_session_badge_overrides() {
    let yaml = r#"
statusline:
  session_badge:
    suffix: ""
    glyphs:
      blocked: "!"
"#;
    let (config, warnings) = parse_config(yaml);
    assert!(warnings.is_empty());
    assert_eq!(config.statusline.session_badge.suffix, "");
    assert_eq!(config.statusline.session_badge.glyphs.blocked, "!");
    assert_eq!(config.statusline.session_badge.glyphs.working, "🟡");
    assert!(config.statusline.session_badge.enabled);
}
```

### Step 2: RED を確認する

```bash
cargo test session_badge 2>&1 | grep -E "^(error|test result)"
```

コンパイルエラー(`SessionBadgeConfig` 未定義)になることを確認。

### Step 3: GREEN — 実装する

`src/config/mod.rs` に追加(`AgentBadgeConfig` の直後):

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SessionBadgeConfig {
    pub enabled: bool,
    /// グリフ直後に付ける区切り文字列。絵文字は表示幅が広いので
    /// デフォルトで半角スペース 1 個を挟む(バッジ値自体に含める)。
    pub suffix: String,
    pub glyphs: SessionBadgeGlyphs,
}

impl Default for SessionBadgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            suffix: " ".to_string(),
            glyphs: SessionBadgeGlyphs::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SessionBadgeGlyphs {
    pub blocked: String,
    pub working: String,
    pub done: String,
    pub idle: String,
}

impl Default for SessionBadgeGlyphs {
    fn default() -> Self {
        Self {
            blocked: "🔴".to_string(),
            working: "🟡".to_string(),
            done: "🔵".to_string(),
            idle: "🟢".to_string(),
        }
    }
}
```

`StatuslineConfig` にフィールドを足す:

```rust
pub struct StatuslineConfig {
    pub sessions: StatuslineSessionsConfig,
    pub category: StatuslineCategoryConfig,
    pub agent_badge: AgentBadgeConfig,
    pub session_badge: SessionBadgeConfig,
}
```

`src/config/schema.rs` にも既存パターンに合わせて `session_badge`
(enabled / suffix / glyphs.{blocked,working,done,idle})を追記する。
確認: `cargo run --bin vt -- config schema | grep session_badge` が非空。

### Step 4: GREEN を確認する

```bash
cargo test session_badge 2>&1 | grep -E "^(error|test result)"
cargo run --bin vt -- config schema | grep -c session_badge
```

### Step 5: 品質ゲートとコミット

```bash
cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src/config/mod.rs src/config/load.rs src/config/schema.rs
git commit -m "$(cat <<'EOF'
session badge の config を追加する

- statusline.session_badge に enabled/suffix/glyphs を追加
- デフォルトは絵文字 4 色 + 半角スペース suffix
EOF
)"
```

---

## Task 2: PaneSnapshot に window_active / session_attached を追加する

既読判定に必要な 2 フィールドを poll フォーマットへ足す。

### Step 1: RED — テストを書く

`src/options/snapshot.rs` のテストモジュールに追加/修正:

```rust
#[test]
fn snapshot_format_includes_window_active_and_session_attached() {
    let format = snapshot_format();
    assert!(format.contains("#{window_active}"));
    assert!(format.contains("#{session_attached}"));
}

#[test]
fn parse_snapshot_lines_reads_activity_fields() {
    let sep = '\u{1f}';
    // 固定 7 フィールド + sidebar + PANE_STATE_KEYS(10)
    let line = [
        "main", "@1", "%1", "/tmp", "zsh", "1", "2", "", "codex", "running", "", "", "", "",
        "", "", "", "",
    ]
    .join(&sep.to_string());
    let panes = parse_snapshot_lines(&line);
    assert_eq!(panes.len(), 1);
    assert!(panes[0].window_active);
    assert!(panes[0].session_attached);

    let detached = [
        "main", "@1", "%1", "/tmp", "zsh", "0", "0", "", "codex", "running", "", "", "", "",
        "", "", "", "",
    ]
    .join(&sep.to_string());
    let panes = parse_snapshot_lines(&detached);
    assert!(!panes[0].window_active);
    assert!(!panes[0].session_attached);
}
```

### Step 2: RED を確認する

```bash
cargo test snapshot 2>&1 | grep -E "^(error|test result)"
```

### Step 3: GREEN — 実装する

`PaneSnapshot` にフィールドを追加(`current_command` の直後):

```rust
pub struct PaneSnapshot {
    pub session: String,
    pub window_id: String,
    pub pane_id: String,
    pub current_path: String,
    pub current_command: String,
    /// この pane の window がセッションのカレント window か(#{window_active})
    pub window_active: bool,
    /// セッションにクライアントがアタッチされているか(#{session_attached} > 0)
    pub session_attached: bool,
    pub is_sidebar: bool,
    // ... 以降は既存のまま
}
```

`snapshot_format()` の固定フィールドを 5 → 7 に:

```rust
pub fn snapshot_format() -> String {
    let mut fields: Vec<String> = vec![
        "#{session_name}".into(),
        "#{window_id}".into(),
        "#{pane_id}".into(),
        "#{pane_current_path}".into(),
        "#{pane_current_command}".into(),
        "#{window_active}".into(),
        "#{session_attached}".into(),
        format!("#{{{key}}}", key = super::KEY_SIDEBAR_MARKER),
    ];
    fields.extend(PANE_STATE_KEYS.iter().map(|key| format!("#{{{key}}}")));
    fields.join(&FIELD_SEP.to_string())
}
```

`parse_snapshot_lines()` を追随(`expected = 8 + PANE_STATE_KEYS.len()`、
インデックスを 2 ずらす。`session_attached` はクライアント数が返るので
`!fields[6].is_empty() && fields[6] != "0"` で真偽化):

```rust
pub fn parse_snapshot_lines(output: &str) -> Vec<PaneSnapshot> {
    let expected = 8 + PANE_STATE_KEYS.len();
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(FIELD_SEP).collect();
            if fields.len() != expected {
                return None;
            }
            Some(PaneSnapshot {
                session: fields[0].to_string(),
                window_id: fields[1].to_string(),
                pane_id: fields[2].to_string(),
                current_path: fields[3].to_string(),
                current_command: fields[4].to_string(),
                window_active: fields[5] == "1",
                session_attached: !fields[6].is_empty() && fields[6] != "0",
                is_sidebar: fields[7] == "1",
                agent: fields[8].to_string(),
                status: fields[9].to_string(),
                prompt: fields[10].to_string(),
                prompt_source: fields[11].to_string(),
                wait_reason: fields[12].to_string(),
                attention: fields[13].to_string(),
                started_at: fields[14].to_string(),
                completed_at: fields[15].to_string(),
                tasks: fields[16].to_string(),
                subagents: fields[17].to_string(),
            })
        })
        .collect()
}
```

既存テスト・runtime/server/tree などの `PaneSnapshot` 構造体リテラルは
コンパイルエラーに従って `window_active: false, session_attached: false` を追記する
(テストの既定は両方 false = デタッチ scratch と同じ挙動)。

### Step 4: GREEN を確認する

```bash
cargo test 2>&1 | grep -E "^(error|test result)"
```

### Step 5: 品質ゲートとコミット

```bash
cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add -A src/
git commit -m "$(cat <<'EOF'
pane snapshot に window_active と session_attached を追加する

- 既読判定用に #{window_active} / #{session_attached} を poll フォーマットへ追加
- 既存のテスト構築箇所へ両フィールドを追記
EOF
)"
```

---

## Task 3: 4 状態マッピングの純関数モジュールを追加する

### Step 1: RED — テストを書く

新規 `src/daemon/session_badge.rs`(テストのみ先に書き、`src/daemon/mod.rs` に
`pub mod session_badge;` を追加):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SessionBadgeConfig;
    use crate::hook::RollupLevel;

    #[test]
    fn blocked_covers_error_permission_waiting() {
        for level in [
            RollupLevel::Error,
            RollupLevel::Permission,
            RollupLevel::Waiting,
        ] {
            assert_eq!(badge_state(level, false), BadgeState::Blocked);
            assert_eq!(badge_state(level, true), BadgeState::Blocked);
        }
    }

    #[test]
    fn working_covers_running_and_background() {
        assert_eq!(badge_state(RollupLevel::Running, false), BadgeState::Working);
        assert_eq!(badge_state(RollupLevel::Background, true), BadgeState::Working);
    }

    #[test]
    fn idle_splits_by_unread_flag() {
        assert_eq!(badge_state(RollupLevel::Idle, true), BadgeState::Done);
        assert_eq!(badge_state(RollupLevel::Idle, false), BadgeState::Idle);
    }

    #[test]
    fn session_rollup_picks_most_urgent_state() {
        let config = SessionBadgeConfig::default();
        let value = session_badge_value(
            [BadgeState::Idle, BadgeState::Blocked, BadgeState::Working],
            &config,
        );
        assert_eq!(value.as_deref(), Some("🔴 "));
    }

    #[test]
    fn session_badge_value_appends_suffix_and_respects_custom_glyphs() {
        let mut config = SessionBadgeConfig::default();
        config.suffix = "|".to_string();
        config.glyphs.done = "D".to_string();
        let value = session_badge_value([BadgeState::Done], &config);
        assert_eq!(value.as_deref(), Some("D|"));
    }

    #[test]
    fn session_badge_value_is_none_for_no_agents() {
        let config = SessionBadgeConfig::default();
        assert_eq!(session_badge_value([], &config), None);
    }
}
```

### Step 2: RED を確認する

```bash
cargo test session_badge 2>&1 | grep -E "^(error|test result)"
```

### Step 3: GREEN — 実装する

`src/daemon/session_badge.rs`:

```rust
use crate::config::SessionBadgeConfig;
use crate::hook::RollupLevel;

/// statusline sessions の表示 4 状態。
/// 宣言順 = 注意度の高い順(session 集約は min を取る)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BadgeState {
    Blocked,
    Working,
    Done,
    Idle,
}

/// RollupLevel(6 値)を未読フラグ込みで 4 状態へ畳む。
pub fn badge_state(level: RollupLevel, unread: bool) -> BadgeState {
    match level {
        RollupLevel::Error | RollupLevel::Permission | RollupLevel::Waiting => BadgeState::Blocked,
        RollupLevel::Running | RollupLevel::Background => BadgeState::Working,
        RollupLevel::Idle => {
            if unread {
                BadgeState::Done
            } else {
                BadgeState::Idle
            }
        }
    }
}

/// session 内の pane 状態を集約してバッジ文字列(グリフ + suffix)を返す。
/// agent pane が 1 つも無ければ None(バッジを消す)。
pub fn session_badge_value(
    states: impl IntoIterator<Item = BadgeState>,
    config: &SessionBadgeConfig,
) -> Option<String> {
    let state = states.into_iter().min()?;
    let glyph = match state {
        BadgeState::Blocked => &config.glyphs.blocked,
        BadgeState::Working => &config.glyphs.working,
        BadgeState::Done => &config.glyphs.done,
        BadgeState::Idle => &config.glyphs.idle,
    };
    Some(format!("{glyph}{}", config.suffix))
}
```

あわせて `src/sidebar/tree.rs` の `fn rollup_for_pane` を `pub(crate) fn` に昇格する
(runtime から pane 単位の RollupLevel を得るのに再利用。ロジックの二重化を避ける)。

### Step 4: GREEN を確認する

```bash
cargo test session_badge 2>&1 | grep -E "^(error|test result)"
```

### Step 5: 品質ゲートとコミット

```bash
cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src/daemon/session_badge.rs src/daemon/mod.rs src/sidebar/tree.rs
git commit -m "session badge の 4 状態マッピングを追加する"
```

---

## Task 4: runtime に未読追跡と badge sync effect を追加する

### Step 1: RED — テストを書く

`src/daemon/runtime.rs` のテストモジュールに追加。テストヘルパーは既存の
PaneSnapshot 構築ヘルパーがあればそれに合わせる(無ければ以下を使う):

```rust
fn agent_pane(session: &str, pane_id: &str, status: &str) -> PaneSnapshot {
    PaneSnapshot {
        session: session.to_string(),
        window_id: "@1".to_string(),
        pane_id: pane_id.to_string(),
        current_path: "/tmp".to_string(),
        current_command: "zsh".to_string(),
        window_active: false,
        session_attached: false,
        is_sidebar: false,
        agent: "codex".to_string(),
        status: status.to_string(),
        prompt: String::new(),
        prompt_source: String::new(),
        wait_reason: String::new(),
        attention: String::new(),
        started_at: String::new(),
        completed_at: String::new(),
        tasks: String::new(),
        subagents: String::new(),
    }
}

#[test]
fn panes_updated_emits_set_session_badge_effect() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    assert_eq!(
        effects,
        vec![RuntimeEffect::SetSessionBadge {
            session: "main".to_string(),
            value: "🟡 ".to_string(),
        }]
    );
}

#[test]
fn unchanged_badge_emits_no_effect() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    assert!(effects.is_empty());
}

#[test]
fn running_to_idle_becomes_done_until_window_viewed() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    // 完了(デタッチ中)→ 未読 Done = 🔵
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "idle",
    )]));
    assert_eq!(
        effects,
        vec![RuntimeEffect::SetSessionBadge {
            session: "main".to_string(),
            value: "🔵 ".to_string(),
        }]
    );
    // 該当 pane の window を表示(アタッチ + カレント window)→ 既読 Idle = 🟢
    let mut viewed = agent_pane("main", "%1", "idle");
    viewed.window_active = true;
    viewed.session_attached = true;
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![viewed]));
    assert_eq!(
        effects,
        vec![RuntimeEffect::SetSessionBadge {
            session: "main".to_string(),
            value: "🟢 ".to_string(),
        }]
    );
}

#[test]
fn first_seen_idle_pane_is_not_unread() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "idle",
    )]));
    assert_eq!(
        effects,
        vec![RuntimeEffect::SetSessionBadge {
            session: "main".to_string(),
            value: "🟢 ".to_string(),
        }]
    );
}

#[test]
fn session_rollup_prefers_blocked_over_working() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let mut blocked = agent_pane("main", "%2", "waiting");
    blocked.wait_reason = "permission_prompt".to_string();
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![
        agent_pane("main", "%1", "running"),
        blocked,
    ]));
    assert_eq!(
        effects,
        vec![RuntimeEffect::SetSessionBadge {
            session: "main".to_string(),
            value: "🔴 ".to_string(),
        }]
    );
}

#[test]
fn sessions_get_independent_badges() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![
        agent_pane("alpha", "%1", "running"),
        agent_pane("beta", "%2", "idle"),
    ]));
    assert_eq!(
        effects,
        vec![
            RuntimeEffect::SetSessionBadge {
                session: "alpha".to_string(),
                value: "🟡 ".to_string(),
            },
            RuntimeEffect::SetSessionBadge {
                session: "beta".to_string(),
                value: "🟢 ".to_string(),
            },
        ]
    );
}

#[test]
fn vanished_session_emits_clear_effect() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![]));
    assert_eq!(
        effects,
        vec![RuntimeEffect::ClearSessionBadge {
            session: "main".to_string(),
        }]
    );
}

#[test]
fn shutdown_clears_all_written_badges() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    let effects = state.apply_event(DaemonEvent::Shutdown);
    assert_eq!(
        effects,
        vec![RuntimeEffect::ClearSessionBadge {
            session: "main".to_string(),
        }]
    );
    assert!(!state.is_running());
}

#[test]
fn disabled_config_writes_no_badges() {
    let mut config = Config::default();
    config.statusline.session_badge.enabled = false;
    let mut state = RuntimeState::new(config, SidebarState::default());
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    assert!(effects.is_empty());
}

#[test]
fn sidebar_and_agentless_panes_are_ignored() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let mut sidebar = agent_pane("main", "%9", "running");
    sidebar.is_sidebar = true;
    let mut plain = agent_pane("main", "%8", "");
    plain.agent = String::new();
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![sidebar, plain]));
    assert!(effects.is_empty());
}
```

### Step 2: RED を確認する

```bash
cargo test runtime 2>&1 | grep -E "^(error|test result)"
```

### Step 3: GREEN — 実装する

`src/daemon/runtime.rs` の変更点:

`RuntimeEffect` に 2 バリアント追加:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEffect {
    JumpPane(String),
    SaveState(SidebarState),
    SetSessionBadge { session: String, value: String },
    ClearSessionBadge { session: String },
}
```

`RuntimeState` にフィールド追加(`new()` でも空初期化):

```rust
pub struct RuntimeState {
    // ... 既存フィールド
    /// pane_id → 直近の観測が Idle だったか(非 Idle → Idle 遷移の検出用)
    pane_was_idle: BTreeMap<String, bool>,
    /// pane_id → 未読(Done)フラグ
    unread: BTreeMap<String, bool>,
    /// session 名 → 直近に書き込んだバッジ値(差分書き込み用)
    written_badges: BTreeMap<String, String>,
}
```

`apply_event` の `PanesUpdated` / `Shutdown` を変更:

```rust
DaemonEvent::PanesUpdated(panes) => {
    self.panes = panes;
    self.update_unread();
    self.rebuild_snapshot();
    self.broadcast_if_needed();
    self.sync_session_badges()
}
// ...
DaemonEvent::Shutdown => {
    self.running = false;
    self.clients.values().for_each(|slot| slot.close());
    let effects = self
        .written_badges
        .keys()
        .map(|session| RuntimeEffect::ClearSessionBadge {
            session: session.clone(),
        })
        .collect();
    self.written_badges.clear();
    effects
}
```

未読追跡と差分 sync(`impl RuntimeState` にプライベートメソッド追加):

```rust
fn update_unread(&mut self) {
    let mut next_was_idle = BTreeMap::new();
    let mut next_unread = BTreeMap::new();
    for pane in self.panes.iter().filter(|p| !p.is_sidebar && !p.agent.is_empty()) {
        let level = crate::sidebar::tree::rollup_for_pane(pane);
        let is_idle = level == crate::hook::RollupLevel::Idle;
        let was_idle = self.pane_was_idle.get(&pane.pane_id).copied();
        let mut unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
        match was_idle {
            // 初見 pane は未読にしない(daemon 起動直後に青一色になるのを防ぐ)
            None => unread = false,
            // 非 Idle → Idle 遷移 = 完了。未読を立てる
            Some(false) if is_idle => unread = true,
            _ => {}
        }
        if !is_idle {
            unread = false;
        }
        // アタッチ中クライアントのカレント window に映っていれば既読
        if pane.window_active && pane.session_attached {
            unread = false;
        }
        next_was_idle.insert(pane.pane_id.clone(), is_idle);
        next_unread.insert(pane.pane_id.clone(), unread);
    }
    // 消えた pane のエントリはここで自然に破棄される
    self.pane_was_idle = next_was_idle;
    self.unread = next_unread;
}

fn sync_session_badges(&mut self) -> Vec<RuntimeEffect> {
    use crate::daemon::session_badge::{badge_state, session_badge_value};
    let badge_config = &self.config.statusline.session_badge;
    let mut desired: BTreeMap<String, String> = BTreeMap::new();
    if badge_config.enabled {
        let mut states: BTreeMap<String, Vec<crate::daemon::session_badge::BadgeState>> =
            BTreeMap::new();
        for pane in self.panes.iter().filter(|p| !p.is_sidebar && !p.agent.is_empty()) {
            let level = crate::sidebar::tree::rollup_for_pane(pane);
            let unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
            states
                .entry(pane.session.clone())
                .or_default()
                .push(badge_state(level, unread));
        }
        for (session, list) in states {
            if let Some(value) = session_badge_value(list, badge_config) {
                desired.insert(session, value);
            }
        }
    }
    let mut effects = Vec::new();
    for (session, value) in &desired {
        if self.written_badges.get(session) != Some(value) {
            effects.push(RuntimeEffect::SetSessionBadge {
                session: session.clone(),
                value: value.clone(),
            });
        }
    }
    for session in self.written_badges.keys() {
        if !desired.contains_key(session) {
            effects.push(RuntimeEffect::ClearSessionBadge {
                session: session.clone(),
            });
        }
    }
    self.written_badges = desired;
    effects
}
```

注意: `current_fingerprint` は変更しない(バッジは sidebar への push 対象ではなく
tmux option への副作用)。

### Step 4: GREEN を確認する

```bash
cargo test runtime 2>&1 | grep -E "^(error|test result)"
```

### Step 5: 品質ゲートとコミット

```bash
cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src/daemon/runtime.rs
git commit -m "$(cat <<'EOF'
runtime に session badge の未読追跡と差分 sync を追加する

- 非 Idle → Idle 遷移で未読、カレント window 表示で既読
- 変化した session のみ Set/ClearSessionBadge effect を返す
- Shutdown で書き込み済みバッジを全消去
EOF
)"
```

---

## Task 5: WorkerIo 経由で tmux へ書き込む

### Step 1: RED — テストを書く

`src/daemon/server.rs` の既存 runtime loop テスト(`LoopWorkerIo`)に
session option 記録を追加し、新テストを書く:

```rust
#[test]
fn runtime_loop_executes_session_badge_effects() {
    // LoopWorkerIo に session_options: Mutex<Vec<(String, String, Option<String>)>> を追加し
    // set_session_option / unset_session_option の呼び出しを記録する。
    // (String, String, Option<String>) = (session, key, Some(value)=set / None=unset)
    let worker_io = Arc::new(LoopWorkerIo::default());
    let (tx, rx) = mpsc::channel();
    let state = RuntimeState::new(Config::default(), SidebarState::default());
    let handle = {
        let worker_io = worker_io.clone();
        thread::spawn(move || run_runtime_loop(state, rx, None, worker_io))
    };
    tx.send(DaemonEvent::PanesUpdated(vec![test_agent_pane(
        "main", "%1", "running",
    )]))
    .unwrap();
    tx.send(DaemonEvent::Shutdown).unwrap();
    handle.join().unwrap().unwrap();

    let calls = worker_io.session_options.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![
            (
                "main".to_string(),
                "@vde_session_status".to_string(),
                Some("🟡 ".to_string())
            ),
            ("main".to_string(), "@vde_session_status".to_string(), None),
        ]
    );
}
```

### Step 2: RED を確認する

```bash
cargo test 2>&1 | grep -E "^(error|test result)"
```

コンパイルエラー(trait メソッド未定義)になることを確認。

### Step 3: GREEN — 実装する

`src/options/mod.rs` にキー定数を追加(session scope の並び):

```rust
/// session scope: statusline sessions のバッジ(writer は daemon のみ)
pub const KEY_SESSION_STATUS: &str = "@vde_session_status";
```

`set_session_option` / `unset_session_option` ヘルパーは既存のものをそのまま使う。

`src/daemon/workers.rs` の `WorkerIo` trait にメソッド追加:

```rust
pub trait WorkerIo: Send + Sync + 'static {
    fn read_panes(&self) -> Result<Vec<PaneSnapshot>>;
    fn capture_tail(&self, pane_id: &str) -> Result<String>;
    fn jump_to_pane(&self, pane_id: &str) -> Result<()>;
    fn set_session_option(&self, session: &str, key: &str, value: &str) -> Result<()>;
    fn unset_session_option(&self, session: &str, key: &str) -> Result<()>;
}
```

`SystemWorkerIo` の impl は `crate::options::set_session_option` /
`crate::options::unset_session_option` へ委譲する。テスト用 `MockWorkerIo` と
server.rs の `LoopWorkerIo` にも記録付きで実装する。

`src/daemon/server.rs` の `handle_runtime_effects` に追加
(JumpPane と同様、失敗しても daemon は落とさない):

```rust
RuntimeEffect::SetSessionBadge { session, value } => {
    if let Err(error) =
        worker_io.set_session_option(&session, crate::options::KEY_SESSION_STATUS, &value)
    {
        eprintln!("[vde-tmux] session badge set failed: {error:#}");
    }
}
RuntimeEffect::ClearSessionBadge { session } => {
    if let Err(error) =
        worker_io.unset_session_option(&session, crate::options::KEY_SESSION_STATUS)
    {
        eprintln!("[vde-tmux] session badge clear failed: {error:#}");
    }
}
```

注意: session が既に kill されている場合 set/unset は失敗するが、
eprintln のみで握りつぶす(session 消滅時の Clear で該当しやすい)。

### Step 4: GREEN を確認する

```bash
cargo test 2>&1 | grep -E "^(error|test result)"
```

### Step 5: 品質ゲートとコミット

```bash
cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src/options/mod.rs src/daemon/workers.rs src/daemon/server.rs
git commit -m "$(cat <<'EOF'
session badge を daemon から tmux へ書き込む

- @vde_session_status キーを追加
- WorkerIo に session option の set/unset を追加
- runtime effect を server ループで実行(失敗は eprintln のみ)
EOF
)"
```

---

## Task 6: statusline-sessions がバッジを描画する

### Step 1: RED — テストを書く

`src/session/mod.rs` のテストに追加/修正:

```rust
#[test]
fn session_list_format_includes_session_status() {
    assert!(session_list_format().contains("#{@vde_session_status}"));
}

#[test]
fn parse_sessions_reads_badge_field() {
    let sep = '\u{1f}';
    let line = ["main", "1", "1700000000", "misc", "/tmp", "", "🔴 "]
        .join(&sep.to_string());
    let sessions = parse_sessions(&line);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].badge, "🔴 ");
}
```

`src/statusline/mod.rs` のテストに追加(既存の render テストのヘルパーに合わせて調整):

```rust
#[test]
fn render_statusline_sessions_prefixes_badge_to_label() {
    // SessionInfo { badge: "🔴 ".into(), .. } の session を含めて
    // render_statusline_sessions を呼び、出力に "🔴 " + session ラベルが
    // 連続して現れることを検証する。badge が空の session は従来表示のまま。
}
```

### Step 2: RED を確認する

```bash
cargo test 2>&1 | grep -E "^(error|test result)"
```

### Step 3: GREEN — 実装する

`src/session/mod.rs`:

- `SessionInfo` に `pub badge: String` を追加。
- `session_list_format()` の末尾に `"#{@vde_session_status}"` を追加(7 フィールド)。
- `parse_sessions()` の `expected` を 7 にし、`badge: fields[6].to_string()` を追加。
- 既存の `SessionInfo` 構造体リテラル(テスト含む)はコンパイルエラーに従って
  `badge: String::new()` を追記する。

`src/statusline/mod.rs`:

- `render_session_segment` に `badge: &str` 引数を追加し、ラベルの先頭に前置する
  (badge は suffix 込みなので追加の区切りは不要):

```rust
fn render_session_segment(
    style: &SegmentStyle,
    badge: &str,
    session_name: &str,
    index: usize,
    show_index: bool,
) -> String {
    let label = if show_index {
        format!("{}:{session_name}", index + 1)
    } else {
        session_name.to_string()
    };
    let label = format!("{badge}{label}");
    let body = style
        .format
        .replace("{session}", &label)
        .replace("{index}", &(index + 1).to_string());
    tmux_style_segment(style, &body)
}
```

- `render_statusline_sessions` の呼び出し側で `&session.badge` を渡す。
- 表示例(show_index 有効時): `🔴 1:api 🟡 2:web 3:docs`
  (agent の居ない session はバッジ無しで従来どおり)。

注意: statusline-sessions は 1 秒間隔(`status-interval`)で tmux が再実行するため、
バッジの反映遅延は最大で「daemon の poll_ms + status-interval」となる。許容範囲。

### Step 4: GREEN を確認する

```bash
cargo test 2>&1 | grep -E "^(error|test result)"
```

### Step 5: 品質ゲートとコミット

```bash
cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src/session/mod.rs src/statusline/mod.rs
git commit -m "$(cat <<'EOF'
statusline sessions に session badge を描画する

- session list フォーマットに @vde_session_status を追加
- 各 session ラベルの先頭にバッジを前置(suffix 込み)
EOF
)"
```

---

## Task 7: smoke・ドキュメント

### Step 1: smoke スクリプトを拡張する

`scripts/smoke-m6-runtime.sh` の `capture detect ok` ステップの後に追加
(既存の隔離規約を維持: 隔離 socket、`trap cleanup EXIT`、
cleanup での scratch tmux socket ファイル削除):

```bash
# --- session badge ---
badge_wait() {
  local expect="$1"
  local got=""
  for _ in $(seq 1 50); do
    got="$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_session_status 2>/dev/null || true)"
    [[ "$got" == "$expect" ]] && return 0
    sleep 0.1
  done
  echo "session badge mismatch: expected [$expect] got [$got]" >&2
  return 1
}

# permission prompt 検知中 → Blocked
badge_wait "🔴 "
echo "session badge blocked ok"

# idle へ遷移(デタッチ中なので未読 Done)。
# 重要: 画面に permission prompt の文字列が残っていると layer-3 検知が
# wait_reason を再度 permission に戻し 🔴 のままになるため、先に画面を消す。
tmux -L "$TMUX_SOCKET" send-keys -t "$PANE_ID" C-c
sleep 0.3
tmux -L "$TMUX_SOCKET" send-keys -t "$PANE_ID" "clear" C-m
tmux -L "$TMUX_SOCKET" set-option -p -t "$PANE_ID" @vde_status idle
tmux -L "$TMUX_SOCKET" set-option -p -t "$PANE_ID" -u @vde_wait_reason 2>/dev/null || true
badge_wait "🔵 "
echo "session badge done ok"

# statusline-sessions の出力にバッジが載ることを確認
SESSIONS_OUT="$(VDE_TMUX_SOCKET_NAME="$TMUX_SOCKET" \
  VDE_DAEMON_SOCKET="$DAEMON_SOCKET" \
  XDG_STATE_HOME="$STATE_HOME" \
  "$BIN" statusline-sessions 2>/dev/null || true)"
case "$SESSIONS_OUT" in
  *"🔵 "*) echo "statusline badge render ok" ;;
  *)
    echo "statusline output missing badge: [$SESSIONS_OUT]" >&2
    exit 1
    ;;
esac
```

注意: `statusline-sessions` がカレント session の特定にクライアント情報を必要とし、
デタッチ状態で失敗する場合は、この確認を `render_statusline_sessions` の
ユニットテスト(Task 6)に委ねて smoke からは省いてよい(省いた場合はその旨を
docs/e2e-smoke.md に明記する)。

daemon 停止後(cleanup 前)にバッジが消えることも確認する:

```bash
kill "$DAEMON_PID"
wait "$DAEMON_PID" 2>/dev/null || true
DAEMON_PID=""
BADGE_AFTER="$(tmux -L "$TMUX_SOCKET" show-options -v -t main @vde_session_status 2>/dev/null || true)"
[[ -z "$BADGE_AFTER" ]]
echo "session badge cleanup ok"
```

実行:

```bash
bash scripts/smoke-m6-runtime.sh
```

Expected(追加分を含む全行。ステップ順はスクリプトの実装順に合わせてよいが、
Expected と実装は一致させること):

```
subscribe snapshot ok
capture detect ok
session badge blocked ok
session badge done ok
statusline badge render ok
input redraw state ok
query response ok
session badge cleanup ok
M6 runtime smoke ok
```

### Step 2: ドキュメント更新

- `README.md` の Option Bus 節の session 行に `@vde_session_status` を追記し、
  「writer は daemon のみ。graceful shutdown で削除される」と明記する。
- `README.md` の機能節に session badge を 1 行追加(4 色の意味と既読ルール)。
- `docs/e2e-smoke.md` に今回の smoke 実行記録を M6 表記で追記する。

### Step 3: dotfiles 変更の要否確認(diff 提示のみ、コミット禁止)

status-left は既に `#(vtm statusline-sessions --show-index ...)` を呼んでいるため、
**バッジ表示のための dotfiles 変更は不要**(M7 で `vtm` → `vt` に切り替われば
自動的にバッジが載る)。切替前に本番 tmux で見た目を試したい場合のみ、
status-left の該当部分を一時的に `#(~/repos/github.com/yuki-yano/vde-tmux/target/debug/vt statusline-sessions --show-index)`
へ差し替える diff を提示する(適用の判断・コミットはユーザーが行う)。

### Step 4: 品質ゲートとコミット

```bash
cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
bash scripts/smoke-m6-runtime.sh
git add scripts/smoke-m6-runtime.sh README.md docs/e2e-smoke.md
git commit -m "$(cat <<'EOF'
session badge の smoke とドキュメントを追加する

- smoke に blocked/done/render/cleanup の badge 検証を追加
- README の Option Bus と機能一覧に @vde_session_status を追記
EOF
)"
```

---

## DoD(Definition of Done)

### 機能完了条件

- [ ] agent pane を含む session の `@vde_session_status` に 4 色バッジ(グリフ + suffix)が書かれる
- [ ] `vt statusline-sessions` の各 session ラベル先頭にバッジが表示される(バッジ無し session は従来表示)
- [ ] 状態マッピングが決定事項どおり: 🔴=Error/Permission/Waiting、🟡=Running/Background、🔵=Idle 未読、🟢=Idle 既読
- [ ] session 集約は最も注意度の高い pane の状態(Blocked < Working < Done < Idle の min)
- [ ] 非 Idle → Idle 遷移で未読になり、その pane の window をアタッチ中クライアントで表示すると既読になる
- [ ] daemon 起動直後に初めて観測した Idle pane は未読にならない
- [ ] agent pane が無くなった session のバッジは削除される
- [ ] graceful shutdown(SIGTERM/SIGINT)で全バッジが削除される
- [ ] `statusline.session_badge.enabled: false` で一切書き込まない(既存分は削除)
- [ ] glyphs / suffix が config で変更でき、デフォルトは絵文字 + 半角スペース
- [ ] `@vde_session_status` の writer は daemon のみ(他経路からの書き込みが無いことを grep で確認)

### テスト完了条件

- [ ] Task 1〜6 の全ユニットテストが green(config 2、snapshot 2、session_badge 6、runtime 10、server 1、session/statusline 3 以上)
- [ ] `cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check` が全て green
- [ ] `scripts/smoke-m6-runtime.sh` が scratch tmux で pass(badge blocked/done/cleanup、可能なら render の検証を含む)
- [ ] smoke 実行後に scratch tmux socket・/tmp 残骸が残っていない

### 運用反映条件

- [ ] README(Option Bus / 機能一覧)と docs/e2e-smoke.md が更新されている
- [ ] `vt config schema` の出力に session_badge が含まれる
- [ ] dotfiles 変更が不要であることが確認されている(先行試用する場合のみ diff 提示。適用・コミットはユーザー判断)
- [ ] 本計画のコミットが全てタスク粒度で main に積まれている(push はしない)

## リスク・既知の限界

- daemon がクラッシュした場合、最後に書いたバッジが tmux に残る(graceful shutdown のみ対応)。
- 未読情報は daemon のインメモリのみ。daemon 再起動で全 pane が既読からスタートする。
- バッジの反映遅延は最大「daemon の poll_ms(既定 1000ms)+ tmux status-interval」。
- 絵文字グリフの表示幅はターミナル/フォント依存。suffix デフォルト 1 スペースで大半の環境は
  読めるが、崩れる環境では config で `●` 等 + tmux スタイルタグ入り文字列
  (例 `#[fg=red]●#[default]`)に切り替えられる。statusline-sessions の出力は
  status-left の `#()` 展開後にスタイル解釈されるため機能する。
- poll 周期より速い状態変化は最終状態のみ反映される。
- 旧 vtm の statusline-sessions と新 vt の statusline-sessions を同時に status-left へ
  置かないこと(バッジは新側のみが解釈する。並走 dogfood 中は片方に揃える)。
