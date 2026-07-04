# Plan 12: sidebar runtime followups

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** plan-11 後の残課題として、stale agent 除外、config rename、共通 badge glyph、sidebar glyph UI、click dispatch、preview を実装する。

**Architecture:** agent 生存判定は `PaneSnapshot` 入口の純関数へ集約し、tree / statusline session badge / rollup が同じフィルタを参照する。badge glyph は top-level `badge.glyphs` を唯一の設定元にし、statusline と sidebar は `BadgeState` を共有する。preview は既存 floating pane を維持し、対象 pane 幅・中央配置・scrollback pager を tmux runner 経由で実行する。

**Tech Stack:** Rust、serde_yaml_ng、serde_json schema、tmux、ratatui/crossterm、既存 `MockTmuxRunner`。

---

## DoD

### 機能完了条件

- [ ] `categories.rules[].ghq_patterns` を廃止し、`path_patterns` だけを受け付ける。
- [ ] `${VAR}` 展開対象が `path_patterns` と `session_name_rules[].patterns` になる。
- [ ] `@vde_agent` が残っていても `pane_current_command` が `claude` / `codex` / `opencode` でなければ tree、session badge、rollup から除外される。
- [ ] `badge.glyphs.{blocked,working,done,idle}` が statusline session badge と sidebar 行頭/rail glyph の共通設定になる。
- [ ] Chat 行は行頭 glyph を表示し、行末の `[running]` などの状態テキストを出さない。
- [ ] Repo/Category 行は配下の `BadgeState::min()` glyph を行頭表示し、`[running:3]` の count は維持する。
- [ ] ダブルクリックは 1 回目の preview/toggle を 250ms 保留し、同一行 2 回目で jump する。
- [ ] Category/Repo は pane を持たないため従来どおり即 toggle する。
- [ ] `sidebar.colors.selection_active_bg` / `attention` は削除し、schema/README からも消す。
- [ ] Enter で Detail 行を activate した場合も preview が開く。
- [ ] running subagent の Detail 行を `├` / `└` connector 付きで表示する。
- [ ] preview は対象 agent pane と同じ幅で window 中央に出し、scrollback を含めた `less -R +G` 表示になり、q/Esc で閉じられる。
- [ ] `sidebar.preview.history_lines` で scrollback 行数を指定でき、既定値は 2000。
- [ ] session-manager は plan-11 の重複番号を解消し、`vt session-manager --popup` は `display-popup` 固定として維持する。

### テスト完了条件

- [ ] 各 task で RED を確認してから GREEN 実装する。
- [ ] `rtk cargo test config` が green。
- [ ] `rtk cargo test category` が green。
- [ ] `rtk cargo test daemon` が green。
- [ ] `rtk cargo test sidebar` が green。
- [ ] `rtk cargo fmt && rtk cargo test && rtk cargo clippy --all-targets -- -D warnings && rtk cargo fmt --check` が green。
- [ ] `rtk bash scripts/smoke-m6-runtime.sh` が pass。

### 運用反映条件

- [ ] scratch tmux だけで stale agent 除外、glyph 表示、double click、preview、path_patterns を確認する。
- [ ] 実機確認結果を `docs/e2e-smoke.md` に記録する。
- [ ] 本番 tmux、本番 daemon、本番 socket、dotfiles、旧 2 リポジトリは変更しない。
- [ ] タスクごとに日本語 commit を作成し、未コミット差分を残さない。
- [ ] 完了報告でユーザー実 config への影響を明記する: `ghq_patterns` は `path_patterns` へ要変更。`statusline.session_badge.glyphs` は廃止され `badge.glyphs` へ移動。現状 glyphs 未設定なら影響なし見込み。

## Task 0: session-manager popup fixed の番号整理

**Files:**
- Delete: `docs/plans/2026-07-04-plan-11-session-manager-floating-popup.md`
- Create: `docs/plans/2026-07-05-plan-12-sidebar-runtime-followups.md`
- Modify: `src/session_manager/mod.rs`
- Modify: `docs/e2e-smoke.md`

- [ ] **Step 1: RED**

`src/session_manager/mod.rs` に、popup が version probe / floating pane を使わないことを固定するテストを置く。

```rust
#[test]
fn popup_uses_display_popup_directly() {
    let mock = MockTmuxRunner::new();
    let command = popup_shell_command();
    mock.stub(&["display-popup", "-E", "-w", "80%", "-h", "70%", &command], "");

    open_popup(&mock).unwrap();

    assert_eq!(mock.calls().len(), 1);
    assert_eq!(mock.calls()[0][0], "display-popup");
}
```

Run:

```bash
rtk cargo test session_manager::tests::popup_uses_display_popup_directly
```

Expected: 旧 floating pane 実装なら FAIL。

- [ ] **Step 2: GREEN**

`open_popup` は `display-popup` だけを呼ぶ。

```rust
pub fn open_popup(runner: &dyn TmuxRunner) -> Result<()> {
    open_display_popup(runner)?;
    Ok(())
}

fn open_display_popup(runner: &dyn TmuxRunner) -> Result<()> {
    let command = popup_shell_command();
    runner.run(&["display-popup", "-E", "-w", "80%", "-h", "70%", &command])?;
    Ok(())
}
```

Run:

```bash
rtk cargo test session_manager
```

Expected: PASS。

- [ ] **Step 3: Commit**

```bash
rtk git add src/session_manager/mod.rs docs/e2e-smoke.md docs/plans/2026-07-05-plan-12-sidebar-runtime-followups.md
rtk git commit -m "$(cat <<'EOF'
session-manager popup 固定を Plan 12 に整理する

- 重複していた plan-11 session-manager 文書を plan-12 へ統合
- vt session-manager --popup は display-popup 固定に戻す
EOF
)"
```

## Task 1: config rename と共通 badge glyph

**Files:**
- Modify: `src/config/mod.rs`
- Modify: `src/config/load.rs`
- Modify: `src/config/schema.rs`
- Modify: `src/category/mod.rs`
- Modify: `src/daemon/session_badge.rs`
- Modify: `README.md`
- Modify: `docs/migration.md`

- [ ] **Step 1: RED**

`src/config/mod.rs` / `src/config/load.rs` / `src/category/mod.rs` に次のテストを追加する。

```rust
#[test]
fn categories_section_parses_path_patterns_only() {
    let yaml = r#"
categories:
  rules:
    - category: work
      path_patterns:
        - github.com/${WORK_OWNER}/*
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.categories.rules[0].path_patterns[0], "github.com/${WORK_OWNER}/*");
    let err = serde_yaml_ng::from_str::<Config>(
        "categories:\n  rules:\n    - category: work\n      ghq_patterns:\n        - github.com/acme/*\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("ghq_patterns"));
}

#[test]
fn config_pattern_env_expands_path_patterns() {
    let loaded = parse_config_with_env(
        "categories:\n  rules:\n    - category: work\n      path_patterns:\n        - github.com/${WORK_OWNER}/*\n",
        &env(&[("WORK_OWNER", "acme")]),
    );
    assert!(loaded.warnings.is_empty());
    assert_eq!(loaded.config.categories.rules[0].path_patterns[0], "github.com/acme/*");
}

#[test]
fn project_rule_matches_path_patterns_suffix_with_star() {
    let mut config = Config::default();
    config.categories.rules.push(CategoryRule {
        category: "work".to_string(),
        path_patterns: vec!["github.com/acme/*".to_string()],
    });
    let actual = resolve_category_for_session(
        &config,
        &session("app", "/Users/me/src/github.com/acme/app", "", ""),
    );
    assert_eq!(actual, "work");
}
```

`src/config/mod.rs` / `src/daemon/session_badge.rs` に共通 glyph の RED も追加する。

```rust
#[test]
fn badge_glyphs_are_top_level_config() {
    let config: Config = serde_yaml_ng::from_str(
        "badge:\n  glyphs:\n    working: W\nstatusline:\n  session_badge:\n    suffix: \"\"\n",
    )
    .unwrap();
    assert_eq!(config.badge.glyphs.working, "W");
    assert_eq!(config.statusline.session_badge.suffix, "");

    let err = serde_yaml_ng::from_str::<Config>(
        "statusline:\n  session_badge:\n    glyphs:\n      working: W\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("glyphs"));
}
```

Run:

```bash
rtk cargo test config category daemon::session_badge
```

Expected: FAIL。`path_patterns` / `badge` が未定義。

- [ ] **Step 2: GREEN**

`Config` に `badge` を追加し、`CategoryRule` は `deny_unknown_fields` で `path_patterns` のみを受ける。

```rust
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub categories: CategoriesConfig,
    pub statusline: StatuslineConfig,
    pub sidebar: SidebarConfig,
    pub daemon: DaemonConfig,
    pub badge: BadgeConfig,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CategoryRule {
    pub category: String,
    pub path_patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct BadgeConfig {
    pub glyphs: BadgeGlyphs,
}

impl Default for BadgeConfig {
    fn default() -> Self {
        Self { glyphs: BadgeGlyphs::default() }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct BadgeGlyphs {
    pub blocked: String,
    pub working: String,
    pub done: String,
    pub idle: String,
}

impl Default for BadgeGlyphs {
    fn default() -> Self {
        Self {
            blocked: "🔴".to_string(),
            working: "🟡".to_string(),
            done: "🔵".to_string(),
            idle: "🟢".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionBadgeConfig {
    pub enabled: bool,
    pub suffix: String,
}
```

`load.rs` は `path_patterns` を展開する。

```rust
for (rule_index, rule) in config.categories.rules.iter_mut().enumerate() {
    for (pattern_index, pattern) in rule.path_patterns.iter_mut().enumerate() {
        *pattern = expand_pattern(
            pattern,
            env,
            &format!("categories.rules.{rule_index}.path_patterns.{pattern_index}"),
        )?;
    }
}
```

`category/mod.rs` は `path_patterns` を参照する。

```rust
for rule in &config.categories.rules {
    if rule
        .path_patterns
        .iter()
        .any(|pattern| matches_path_pattern(pattern, &session.project_path))
    {
        return rule.category.clone();
    }
}
```

`session_badge_value` は suffix と glyph を分ける。

```rust
pub fn session_badge_value(
    states: impl IntoIterator<Item = BadgeState>,
    glyphs: &BadgeGlyphs,
    suffix: &str,
) -> Option<String> {
    let state = states.into_iter().min()?;
    let glyph = glyph_for_state(state, glyphs);
    Some(format!("{glyph}{suffix}"))
}

pub fn glyph_for_state<'a>(state: BadgeState, glyphs: &'a BadgeGlyphs) -> &'a str {
    match state {
        BadgeState::Blocked => &glyphs.blocked,
        BadgeState::Working => &glyphs.working,
        BadgeState::Done => &glyphs.done,
        BadgeState::Idle => &glyphs.idle,
    }
}
```

Run:

```bash
rtk cargo test config category daemon::session_badge
```

Expected: PASS。

- [ ] **Step 3: Docs/schema**

`vt config schema` は top-level `badge.glyphs` と `categories.rules[].path_patterns` を出す。README / migration の config 例は `ghq_patterns` を `path_patterns` へ置換し、`statusline.session_badge.glyphs` を削除して `badge.glyphs` を追加する。

- [ ] **Step 4: Commit**

```bash
rtk git add src/config/mod.rs src/config/load.rs src/config/schema.rs src/category/mod.rs src/daemon/session_badge.rs README.md docs/migration.md
rtk git commit -m "$(cat <<'EOF'
config の path_patterns と badge.glyphs へ移行する

- ghq_patterns と statusline.session_badge.glyphs を廃止
- path_patterns の環境変数展開と schema/docs を更新
EOF
)"
```

## Task 2: stale agent の一括除外

**Files:**
- Modify: `src/options/snapshot.rs`
- Modify: `src/daemon/mod.rs`
- Modify: `src/daemon/runtime.rs`
- Modify: `src/sidebar/tree.rs`
- Modify: `src/daemon/workers.rs`

- [ ] **Step 1: RED**

`src/options/snapshot.rs` に command 判定のテスト、`src/daemon/mod.rs` / `src/daemon/runtime.rs` / `src/sidebar/tree.rs` に stale pane 除外のテストを追加する。

```rust
#[test]
fn detects_agent_process_from_current_command_only_for_real_agent_binary() {
    assert_eq!(detect_agent_from_command("codex"), Some("codex"));
    assert_eq!(detect_agent_from_command("/opt/bin/claude --danger"), Some("claude"));
    assert_eq!(detect_agent_from_command("opencode"), Some("opencode"));
    assert_eq!(detect_agent_from_command("node"), None);
    assert_eq!(detect_agent_from_command("zsh"), None);
}

#[test]
fn stale_agent_option_is_not_counted_in_snapshot() {
    let mut stale = pane("claude", "running", "");
    stale.current_command = "zsh".to_string();
    let snapshot = build_snapshot(&[stale]);
    assert_eq!(snapshot.agent_count, 0);
    assert_eq!(render_agent_badge(&snapshot), "");
}

#[test]
fn stale_agent_option_is_not_rendered_in_sidebar_rows() {
    let mut stale = pane("main", "%1", "/tmp/app", "codex", "running");
    stale.current_command = "node".to_string();
    let rows = build_rows(&Config::default(), &[stale], &SidebarState::default());
    assert!(rows.is_empty());
}

#[test]
fn stale_agent_clears_session_badge() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane("main", "%1", "running")]));
    let mut stale = agent_pane("main", "%1", "running");
    stale.current_command = "zsh".to_string();
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![stale]));
    assert!(effects.iter().any(|effect| matches!(effect, RuntimeEffect::ClearSessionBadge { session } if session == "main")));
}
```

Run:

```bash
rtk cargo test options::snapshot daemon sidebar::tree
```

Expected: FAIL。stale pane が agent として残る。

- [ ] **Step 2: GREEN**

`src/options/snapshot.rs` に生存判定を追加する。

```rust
pub fn detect_agent_from_command(command: &str) -> Option<&'static str> {
    let leaf = command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match leaf.as_str() {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

pub fn is_live_agent_pane(pane: &PaneSnapshot) -> bool {
    !pane.is_sidebar
        && !pane.agent.trim().is_empty()
        && detect_agent_from_command(&pane.current_command).is_some()
}
```

tree / snapshot / runtime は `!pane.agent.is_empty()` の代わりに `is_live_agent_pane(pane)` を使う。

```rust
let panes = panes
    .iter()
    .filter(|pane| crate::options::snapshot::is_live_agent_pane(pane))
    .map(|pane| { /* existing mapping */ })
    .collect::<Vec<_>>();
```

`workers::apply_capture_detection` は stale pane では capture/demote を行わずそのまま返す。

```rust
if !crate::options::snapshot::is_live_agent_pane(&pane) {
    return pane;
}
```

Run:

```bash
rtk cargo test options::snapshot daemon sidebar::tree
```

Expected: PASS。

- [ ] **Step 3: Commit**

```bash
rtk git add src/options/snapshot.rs src/daemon/mod.rs src/daemon/runtime.rs src/sidebar/tree.rs src/daemon/workers.rs
rtk git commit -m "$(cat <<'EOF'
終了済み agent pane を snapshot から除外する

- pane_current_command が agent binary でない stale option を無視
- tree、rollup、session badge の判定を同じ live agent filter に統一
EOF
)"
```

## Task 3: sidebar glyph 表示

**Files:**
- Modify: `src/daemon/session_badge.rs`
- Modify: `src/daemon/runtime.rs`
- Modify: `src/sidebar/tree.rs`
- Modify: `src/sidebar/render.rs`
- Modify: `src/config/mod.rs`

- [ ] **Step 1: RED**

Chat / Repo / rail の glyph と状態テキスト削除をテストする。

```rust
#[test]
fn chat_rows_render_badge_glyph_and_omit_trailing_status_text() {
    let mut row = row("chat::%1", SidebarRowKind::Chat, 0, "codex (%1)", RollupLevel::Running);
    row.badge_state = Some(BadgeState::Working);
    let rendered = render_rows(&[row], &SidebarState::default(), 80);
    assert!(rendered.contains("🟡 codex (%1)"));
    assert!(!rendered.contains("[running]"));
}

#[test]
fn repo_rows_use_min_child_badge_state_but_keep_counts() {
    let row = repo_row_with_badge("app", BadgeState::Blocked, RollupLevel::Running, 3);
    let rendered = render_rows(&[row], &SidebarState::default(), 80);
    assert!(rendered.contains("🔴"));
    assert!(rendered.contains("[running:3]"));
}

#[test]
fn rail_uses_badge_glyphs() {
    let mut row = row("chat::%1", SidebarRowKind::Chat, 0, "codex", RollupLevel::Idle);
    row.badge_state = Some(BadgeState::Done);
    let rendered = render_rows(&[row], &SidebarState::default(), 2);
    assert_eq!(rendered, "🔵");
}
```

Run:

```bash
rtk cargo test sidebar::render sidebar::tree daemon::runtime
```

Expected: FAIL。`SidebarRow` に badge state がない。

- [ ] **Step 2: GREEN**

`BadgeState` を serde 対応し、`SidebarRow` に `badge_state` を載せる。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BadgeState {
    Blocked,
    Working,
    Done,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarRow {
    pub id: String,
    pub kind: SidebarRowKind,
    pub depth: usize,
    pub label: String,
    pub chat_count: usize,
    pub rollup: RollupLevel,
    pub badge_state: Option<BadgeState>,
    pub expanded: bool,
    pub pane_id: Option<String>,
    pub git: Option<crate::git::GitBadge>,
}
```

`AgentPane` に unread / badge state を持たせ、runtime から unread map を渡す。

```rust
pub fn build_rows_with_git_and_unread(
    config: &Config,
    panes: &[PaneSnapshot],
    state: &SidebarState,
    git: &BTreeMap<String, crate::git::GitBadge>,
    unread: &BTreeMap<String, bool>,
) -> Vec<SidebarRow> {
    build_rows_at_with_git_and_unread(config, panes, state, git, unread, now_epoch_secs())
}

let badge_state = crate::daemon::session_badge::badge_state(
    rollup,
    unread.get(&pane.pane_id).copied().unwrap_or(false),
);
```

render は glyph を行頭に入れ、Chat の状態 suffix を消す。

```rust
fn row_badge(row: &SidebarRow, theme: &SidebarRenderTheme) -> String {
    row.badge_state
        .map(|state| theme.glyph_for_badge_state(state).to_string())
        .unwrap_or_default()
}

SidebarRowKind::Chat => {
    let marker = if row.expanded { "v" } else { ">" };
    let glyph = row_badge(row, theme);
    format!("{selected}{indent}{marker} {glyph} {}", row.label)
}
```

Run:

```bash
rtk cargo test sidebar::render sidebar::tree daemon::runtime
```

Expected: PASS。

- [ ] **Step 3: Commit**

```bash
rtk git add src/daemon/session_badge.rs src/daemon/runtime.rs src/sidebar/tree.rs src/sidebar/render.rs src/config/mod.rs
rtk git commit -m "$(cat <<'EOF'
sidebar に session badge glyph を表示する

- Chat/Repo/Category/rail の行頭 glyph を BadgeState から描画
- Chat 行末の状態テキストを削除し、repo/category の count 表示は維持
EOF
)"
```

## Task 4: click dispatch を 250ms 遅延へ変更

**Files:**
- Modify: `src/sidebar/tui.rs`

- [ ] **Step 1: RED**

`ClickTracker` を純粋ロジックでテスト可能にし、Detail single click は期限後 preview、同一行 double click は jump になることを固定する。

```rust
#[test]
fn detail_single_click_is_preview_after_double_click_deadline() {
    let mut tracker = ClickTracker::default();
    let now = Instant::now();
    assert_eq!(
        tracker.register_click(row_ref("detail::%1::status", SidebarRowKind::Detail, Some("%1")), now),
        ClickDecision::Pending
    );
    assert_eq!(
        tracker.flush_due(now + Duration::from_millis(251)),
        Some(ClickAction::PreviewPane("%1".to_string()))
    );
}

#[test]
fn detail_double_click_jumps_without_preview() {
    let mut tracker = ClickTracker::default();
    let now = Instant::now();
    let row = row_ref("detail::%1::status", SidebarRowKind::Detail, Some("%1"));
    assert_eq!(tracker.register_click(row.clone(), now), ClickDecision::Pending);
    assert_eq!(
        tracker.register_click(row, now + Duration::from_millis(120)),
        ClickDecision::Immediate(ClickAction::JumpPane("%1".to_string()))
    );
    assert_eq!(tracker.flush_due(now + Duration::from_millis(251)), None);
}

#[test]
fn repo_click_toggles_immediately() {
    let mut tracker = ClickTracker::default();
    let now = Instant::now();
    assert_eq!(
        tracker.register_click(row_ref("repo::misc::app", SidebarRowKind::Repo, None), now),
        ClickDecision::Immediate(ClickAction::ToggleRow("repo::misc::app".to_string()))
    );
}
```

Run:

```bash
rtk cargo test sidebar::tui::tests::detail_single_click_is_preview_after_double_click_deadline sidebar::tui::tests::detail_double_click_jumps_without_preview sidebar::tui::tests::repo_click_toggles_immediately
```

Expected: FAIL。既存実装は 1 click で即 preview/toggle する。

- [ ] **Step 2: GREEN**

クリック保留構造を導入する。

```rust
const DOUBLE_CLICK_MAX: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClickAction {
    ToggleRow(String),
    PreviewPane(String),
    JumpPane(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClickDecision {
    Immediate(ClickAction),
    Pending,
    None,
}

#[derive(Debug, Clone)]
struct PendingClick {
    row_id: String,
    kind: SidebarRowKind,
    pane_id: Option<String>,
    deadline: Instant,
}

#[derive(Default)]
struct ClickTracker {
    pending: Option<PendingClick>,
}
```

event loop の各 tick で期限切れ action を flush する。

```rust
if let Some(action) = clicks.flush_due(Instant::now()) {
    dispatch_click_action(&context, action);
}
```

`Category` / `Repo` は `pane_id=None` なので pending にしない。Chat / Detail は single action を pending し、同一 row の 2 click で `JumpPane` を即実行する。

Run:

```bash
rtk cargo test sidebar::tui
```

Expected: PASS。

- [ ] **Step 3: Commit**

```bash
rtk git add src/sidebar/tui.rs
rtk git commit -m "$(cat <<'EOF'
sidebar click を double click 判定後に dispatch する

- pane 行の single click action を 250ms 保留する
- 同一行 double click は preview/toggle より jump を優先する
EOF
)"
```

## Task 5: preview floating pane を対象幅・scrollback pager 対応にする

**Files:**
- Modify: `src/config/mod.rs`
- Modify: `src/config/schema.rs`
- Modify: `src/sidebar/tui.rs`
- Modify: `src/daemon/workers.rs`
- Modify: `src/daemon/runtime.rs`
- Modify: `src/daemon/server.rs`
- Modify: `README.md`
- Modify: `docs/migration.md`

- [ ] **Step 1: RED**

preview command の幅・中央配置・履歴行数・lesskey をテストする。

```rust
#[test]
fn preview_config_defaults_history_lines_to_2000() {
    let config = Config::default();
    assert_eq!(config.sidebar.preview.history_lines, 2000);
}

#[test]
fn preview_geometry_uses_target_pane_width_centered_in_window() {
    let geometry = PreviewGeometry::new(100, 40, 64);
    assert_eq!(geometry.width, 64);
    assert_eq!(geometry.x, 18);
    assert_eq!(geometry.height, "80%");
    assert_eq!(geometry.y, "10%");
}

#[test]
fn preview_command_captures_scrollback_and_starts_less_at_bottom() {
    let command = build_preview_command(
        "%26",
        "@1",
        PreviewGeometry::new(100, 40, 64),
        2000,
        Some(std::path::Path::new("/tmp/preview.lesskey")),
    );
    let inner = command.args.last().unwrap();
    assert!(inner.contains("capture-pane -a -p -e -S -2000 -t '%26'"));
    assert!(inner.contains("LESSKEYIN='/tmp/preview.lesskey' less -R +G"));
}

#[test]
fn enter_on_detail_returns_preview_effect() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let mut pane = agent_pane("main", "%1", "running");
    pane.prompt = "prompt".to_string();
    state.ui_state.toggle_expanded("chat::%1");
    state.apply_event(DaemonEvent::PanesUpdated(vec![pane]));
    state.ui_state.selection = Some("detail::%1::status".to_string());
    let effects = state.apply_event(DaemonEvent::Client {
        client_id: ClientId(1),
        event: SidebarClientEvent::Key { key: "enter".to_string() },
    });
    assert!(effects.iter().any(|effect| matches!(
        effect,
        RuntimeEffect::PreviewPane { pane_id, history_lines }
            if pane_id == "%1" && *history_lines == 2000
    )));
}
```

Run:

```bash
rtk cargo test sidebar::tui daemon::runtime config
```

Expected: FAIL。

- [ ] **Step 2: GREEN**

config に preview を追加する。

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub width: SidebarWidth,
    pub min_width: u16,
    pub colors: SidebarColorsConfig,
    pub header: SidebarHeaderConfig,
    pub preview: SidebarPreviewConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SidebarPreviewConfig {
    pub history_lines: u32,
}

impl Default for SidebarPreviewConfig {
    fn default() -> Self {
        Self { history_lines: 2000 }
    }
}
```

preview helper は対象 pane の window/width を問い合わせ、中央位置を計算する。

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewGeometry {
    width: u16,
    x: u16,
    height: String,
    y: String,
}

impl PreviewGeometry {
    fn new(window_width: u16, _window_height: u16, target_width: u16) -> Self {
        let width = target_width.min(window_width.max(1));
        let x = window_width.saturating_sub(width) / 2;
        Self {
            width,
            x,
            height: "80%".to_string(),
            y: "10%".to_string(),
        }
    }
}
```

preview command は scrollback + ANSI + alt-screen fallback + lesskey を使う。

```rust
const LESS_ESCAPE_QUIT_LESSKEY_SRC: &str = "#command\n\\e quit\n";

fn build_preview_inner_command(
    pane_id: &str,
    history_lines: u32,
    less_keyfile: Option<&Path>,
) -> String {
    let target = shell_quote(pane_id);
    let capture = format!(
        "{{ tmux capture-pane -a -p -e -S -{history_lines} -t {target} 2>/dev/null || tmux capture-pane -p -e -S -{history_lines} -t {target}; }}"
    );
    match less_keyfile {
        Some(path) => format!("{capture} | LESSKEYIN={} less -R +G", shell_quote(&path.display().to_string())),
        None => format!("{capture}; printf '\\n-- press any key to close --'; dd bs=1 count=1 >/dev/null 2>&1"),
    }
}
```

runtime / server は preview effect を扱う。

```rust
pub enum RuntimeEffect {
    JumpPane(String),
    PreviewPane {
        pane_id: String,
        history_lines: u32,
    },
    SaveState(SidebarState),
    SetSessionBadge { session: String, value: String },
    ClearSessionBadge { session: String },
}

Some(SidebarCommand::PreviewPane(pane_id)) => {
    return vec![RuntimeEffect::PreviewPane {
        pane_id,
        history_lines: self.config.sidebar.preview.history_lines,
    }];
}
```

`WorkerIo` は `preview_pane(&self, pane_id: &str, history_lines: u32)` を持ち、server は effect を `worker_io.preview_pane` へ渡す。

Run:

```bash
rtk cargo test sidebar::tui daemon config
```

Expected: PASS。

- [ ] **Step 3: Commit**

```bash
rtk git add src/config/mod.rs src/config/schema.rs src/sidebar/tui.rs src/daemon/workers.rs src/daemon/runtime.rs src/daemon/server.rs README.md docs/migration.md
rtk git commit -m "$(cat <<'EOF'
sidebar preview を対象 pane 幅と scrollback 対応にする

- floating pane を対象 pane 幅で中央配置する
- capture-pane の履歴込み出力を less -R +G で表示する
- Detail Enter の preview effect を daemon から実行する
EOF
)"
```

## Task 6: sidebar colors cleanup と subagent detail rows

**Files:**
- Modify: `src/config/mod.rs`
- Modify: `src/config/schema.rs`
- Modify: `src/sidebar/tree.rs`
- Modify: `src/sidebar/render.rs`
- Modify: `README.md`
- Modify: `docs/migration.md`

- [ ] **Step 1: RED**

dead config が unknown field になること、subagent Detail が旧仕様どおり出ることをテストする。

```rust
#[test]
fn sidebar_colors_reject_dead_keys() {
    let err = serde_yaml_ng::from_str::<Config>(
        "sidebar:\n  colors:\n    attention: yellow\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("attention"));
}

#[test]
fn chat_detail_rows_include_running_subagents_with_tree_connectors() {
    let mut agent = pane("main", "%5", "/tmp/app", "claude", "running");
    agent.subagents = "sub12345:Explore|ab120000:general-purpose".to_string();
    let mut state = SidebarState::default();
    state.toggle_expanded("chat::%5");
    let rows = build_rows_at(&Config::default(), &[agent], &state, 1075);
    let labels = rows
        .iter()
        .filter(|row| row.kind == SidebarRowKind::Detail && (row.label.starts_with('├') || row.label.starts_with('└')))
        .map(|row| row.label.as_str())
        .collect::<Vec<_>>();
    assert_eq!(labels, vec!["├ Explore #sub1", "└ general-purpose #ab12"]);
}
```

Run:

```bash
rtk cargo test config sidebar::tree
```

Expected: FAIL。

- [ ] **Step 2: GREEN**

`SidebarColorsConfig` は描画で使うキーだけにする。

```rust
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SidebarColorsConfig {
    pub error: Option<String>,
    pub running: Option<String>,
    pub permission: Option<String>,
    pub background: Option<String>,
    pub waiting: Option<String>,
    pub idle: Option<String>,
    pub selection_bg: Option<String>,
    pub header_active_bg: Option<String>,
    pub header_active_fg: Option<String>,
}
```

subagent decode と Detail 行を追加する。

```rust
fn decode_subagents(raw: &str) -> Vec<(String, String)> {
    raw.split('|')
        .filter_map(|entry| {
            let (id, kind) = entry.split_once(':')?;
            Some((id.to_string(), kind.to_string()))
        })
        .collect()
}

fn subagent_id_suffix(agent_id: &str) -> String {
    let prefix: String = agent_id.chars().take(4).collect();
    if prefix.is_empty() { String::new() } else { format!(" #{prefix}") }
}
```

`push_chat_detail_rows` の session 行と jump 行の間へ subagent 行を入れる。

```rust
let subagents = decode_subagents(&pane.subagents);
if let Some(last_index) = subagents.len().checked_sub(1) {
    for (index, (agent_id, agent_type)) in subagents.iter().enumerate() {
        let connector = if index == last_index { "└" } else { "├" };
        let suffix = subagent_id_suffix(agent_id);
        rows.push(detail_row(
            pane,
            depth,
            &format!("subagent::{index}"),
            format!("{connector} {agent_type}{suffix}"),
        ));
    }
}
```

Run:

```bash
rtk cargo test config sidebar::tree
```

Expected: PASS。

- [ ] **Step 3: Commit**

```bash
rtk git add src/config/mod.rs src/config/schema.rs src/sidebar/tree.rs src/sidebar/render.rs README.md docs/migration.md
rtk git commit -m "$(cat <<'EOF'
sidebar colors の dead key を削除し subagent detail を表示する

- 未使用の attention / selection_active_bg config を廃止
- 実行中 subagent を Detail 行へ connector 付きで表示
EOF
)"
```

## Task 7: scratch tmux smoke と最終品質ゲート

**Files:**
- Modify: `docs/e2e-smoke.md`

- [ ] **Step 1: 実機 smoke**

本番 tmux server / daemon / socket は触らず、scratch tmux と隔離 socket だけを使う。

```bash
rtk cargo build
name="vde-plan12-$(date +%s)"
state_dir="/private/tmp/${name}-state"
config_dir="/private/tmp/${name}-config"
socket_dir="/private/tmp/${name}-daemon"
mkdir -p "$state_dir" "$config_dir/vde/tmux" "$socket_dir"
chmod 700 "$socket_dir"
tmux -L "$name" -f /dev/null new-session -d -s main -n work -c /tmp
trap 'tmux -L "$name" kill-server >/dev/null 2>&1 || true; rm -rf "$state_dir" "$config_dir" "$socket_dir"; rm -f "/private/tmp/tmux-$(id -u)/$name"' EXIT
sock="$socket_dir/daemon.sock"
VDE_TMUX_SOCKET_NAME="$name" XDG_STATE_HOME="$state_dir" XDG_CONFIG_HOME="$config_dir" ./target/debug/vt daemon --socket "$sock" &
```

確認項目:

- stale agent: `@vde_agent=codex` を残した pane の command を `zsh` / `node` にし、sidebar 行と session badge から消えること。
- glyph: running -> idle で `🟡` から `🔵`、window 表示で `🟢` へ変わること。
- click: Detail single click が preview、同一行 double click が jump。
- preview: 対象 pane と同じ幅、中央、scrollback を上へ遡れる、q/Esc で閉じる。
- `path_patterns`: `${WORK_OWNER}` 展開込みで category が解決される。

- [ ] **Step 2: docs 記録**

`docs/e2e-smoke.md` に実行記録を追記する。

```text
Plan 12 sidebar runtime followups smoke も pass。

```text
executed_at=<JST timestamp>
scratch=vde-plan12-<timestamp>
checked=stale agent removed, glyph states, delayed double click, centered scrollback preview, path_patterns env expansion
result=plan12 sidebar runtime followups smoke ok
```
```

- [ ] **Step 3: 品質ゲート**

```bash
rtk cargo fmt
rtk cargo test
rtk cargo clippy --all-targets -- -D warnings
rtk cargo fmt --check
rtk bash scripts/smoke-m6-runtime.sh
```

Expected: 全部 exit 0。

- [ ] **Step 4: Commit**

```bash
rtk git add docs/e2e-smoke.md
rtk git commit -m "$(cat <<'EOF'
Plan 12 の smoke 結果を記録する

- stale agent、glyph、click、preview、path_patterns を scratch tmux で確認
- M6 runtime smoke の再実行結果を確認
EOF
)"
```
