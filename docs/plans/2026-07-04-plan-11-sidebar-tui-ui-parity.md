# Plan 11: sidebar TUI UI parity

旧実装 `~/repos/github.com/yuki-yano/vde-tmux-sidebar/crates/sidebar-cli/src/client.rs` と
`~/repos/github.com/yuki-yano/vde-tmux-sidebar/crates/sidebar-core/src/tree.rs` を仕様書として、新 `vt sidebar attach` の常駐 TUI に UI パリティを実装する。

ヘッダだけはユーザー決定により旧仕様から変える。
旧仕様の「全選択肢を並べてアクティブを背景表示」は採用せず、現在値だけを表示してクリックで次へサイクルする。

## DoD

### 機能完了条件

- [ ] `SidebarRowKind` が `Category / Repo / Chat / Detail / Jump` を持ち、Chat 展開時に `prompt / status / elapsed / session / jump` が表示される。
- [ ] Detail 行は `j/k` の選択対象外、Jump 行は選択対象で、Enter/クリック/preview の挙動が旧テスト相当になる。
- [ ] `status filter` が `all / attn` を持ち、`attn` は `attention=1`、`Error`、`Running`、`Permission` の pane だけを表示する。
- [ ] `v` が `flat -> repo -> category -> flat` で巡回し、既存の `1/2/3` 直接指定も維持される。
- [ ] `J/K` が repo 行の手動順序を変更し、state に永続化される。
- [ ] `p` と Detail 行クリックが Chat/Jump/Detail の対象 pane を floating pane で preview する。
- [ ] Header は rail 幅では非表示、通常幅では `flat|repo|category` と `all|attn` の現在値だけを statusline category 風の固定幅 segment として表示し、クリックで巡回する。`sidebar.header` 設定で pill 風の prefix/suffix/colors/bold も指定できる。
- [ ] 行クリックは header 行数ぶんの offset を補正し、Header クリックと行クリックが混線しない。
- [ ] RollupLevel 色、選択背景、Category/git badge/rail glyph 色が ratatui style で描画される。
- [ ] `sidebar.colors` で旧 `colors.*` 相当を上書きでき、`vt config schema`、README、`docs/migration.md` に反映される。
- [ ] snapshot 未受信時は `connecting to daemon...`、agent 0 件時は `no agents` を描画する。

### テスト完了条件

- [ ] `rtk cargo test sidebar::tree` が RED -> GREEN 済み。
- [ ] `rtk cargo test sidebar::state` が RED -> GREEN 済み。
- [ ] `rtk cargo test sidebar::input` が RED -> GREEN 済み。
- [ ] `rtk cargo test sidebar::render` が RED -> GREEN 済み。
- [ ] `rtk cargo test sidebar::tui` が RED -> GREEN 済み。
- [ ] `rtk cargo test daemon::runtime` が RED -> GREEN 済み。
- [ ] `rtk cargo test config` が RED -> GREEN 済み。
- [ ] 最終品質ゲート `rtk cargo fmt && rtk cargo test && rtk cargo clippy --all-targets -- -D warnings && rtk cargo fmt --check` が green。
- [ ] `rtk bash scripts/smoke-m6-runtime.sh` が pass。

### 運用反映条件

- [ ] scratch tmux だけで `vt hook emit` により running / waiting+permission / idle / attention を持つ pane を作り、色・header cycle・filter・Chat detail・reorder・preview を確認する。
- [ ] 本番 tmux、本番 daemon、本番 socket に触れない。
- [ ] 実機確認結果を `docs/e2e-smoke.md` に追記する。
- [ ] 日本語 commit を、必要なら task 粒度で作成する。

## Task 1: state と tree の行モデルを旧仕様へ寄せる

### RED

`src/sidebar/state.rs` と `src/sidebar/tree.rs` に次のテストを追加する。

```rust
#[test]
fn state_persists_filter_and_manual_order() {
    let state = SidebarState {
        filter: StatusFilter::AttentionOnly,
        manual_order: vec![RepoId::new("misc", "app")],
        ..SidebarState::default()
    };
    let json = serde_json::to_string(&state).unwrap();
    assert!(json.contains(r#""filter":"attention_only""#));
    assert!(json.contains(r#""manual_order""#));
}

#[test]
fn chat_detail_rows_are_hidden_by_default_and_shown_when_toggled_open() {
    let mut pane = pane("main", "%5", "/tmp/app", "codex", "running");
    pane.prompt = "fix the bug".to_string();

    let rows = build_rows_at(&Config::default(), &[pane.clone()], &SidebarState::default(), 1_075);
    assert_eq!(rows.iter().filter(|row| row.kind == SidebarRowKind::Detail).count(), 0);
    assert!(!rows.iter().find(|row| row.id == "chat::%5").unwrap().expanded);

    let mut state = SidebarState::default();
    state.toggle_expanded("chat::%5");
    let rows = build_rows_at(&Config::default(), &[pane], &state, 1_075);

    assert!(rows.iter().any(|row| row.kind == SidebarRowKind::Detail && row.label == "fix the bug"));
    assert!(rows.iter().any(|row| row.kind == SidebarRowKind::Detail && row.label == "status: running"));
    assert!(rows.iter().any(|row| row.kind == SidebarRowKind::Detail && row.label == "elapsed: 1m15s"));
    assert!(rows.iter().any(|row| row.kind == SidebarRowKind::Detail && row.label == "session: main / pane: %5"));
    assert_eq!(rows.last().unwrap().kind, SidebarRowKind::Jump);
}

#[test]
fn attention_only_filter_drops_calm_panes_and_empty_groups() {
    let mut calm = pane("main", "%1", "/tmp/calm", "codex", "idle");
    calm.attention = "0".to_string();
    let active = pane("main", "%2", "/tmp/active", "codex", "running");

    let state = SidebarState {
        filter: StatusFilter::AttentionOnly,
        ..SidebarState::default()
    };
    let rows = build_rows(&Config::default(), &[calm, active], &state);

    assert!(rows.iter().all(|row| !row.id.contains("%1")));
    assert!(rows.iter().any(|row| row.id.contains("%2")));
}
```

RED 確認:

```bash
rtk cargo test sidebar::tree
rtk cargo test sidebar::state
```

### GREEN

`src/sidebar/state.rs` に `RepoId` と `StatusFilter` を追加し、`SidebarState` に `filter` と `manual_order` を持たせる。

```rust
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoId {
    pub category: String,
    pub repo: String,
}

impl RepoId {
    pub fn new(category: impl Into<String>, repo: impl Into<String>) -> Self {
        Self { category: category.into(), repo: repo.into() }
    }

    pub fn from_row_id(id: &str) -> Option<Self> {
        let rest = id.strip_prefix("repo::")?;
        let (category, repo) = rest.split_once("::")?;
        Some(Self::new(category, repo))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusFilter {
    #[default]
    All,
    AttentionOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SidebarState {
    pub version: u64,
    pub view_mode: ViewMode,
    pub filter: StatusFilter,
    pub selection: Option<String>,
    pub collapsed: BTreeSet<String>,
    pub manual_order: Vec<RepoId>,
}
```

`src/sidebar/tree.rs` は `Detail` と `Jump` を行種別に追加し、Chat は既定 closed、Repo/Category は既定 open のままにする。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarRowKind {
    Category,
    Repo,
    Chat,
    Detail,
    Jump,
}

fn chat_row(pane: &AgentPane, depth: usize, state: &SidebarState, now: i64, out: &mut Vec<SidebarRow>) {
    let id = format!("chat::{}", pane.pane_id);
    let expanded = state.is_expanded_with_default(&id, false);
    out.push(SidebarRow {
        id: id.clone(),
        kind: SidebarRowKind::Chat,
        depth,
        label: chat_label(pane),
        chat_count: 1,
        rollup: pane.rollup,
        expanded,
        pane_id: Some(pane.pane_id.clone()),
        git: None,
        attention: pane.attention,
    });
    if expanded {
        push_chat_detail_rows(pane, depth + 1, now, out);
    }
}

fn push_chat_detail_rows(pane: &AgentPane, depth: usize, now: i64, out: &mut Vec<SidebarRow>) {
    if !pane.prompt.trim().is_empty() {
        out.push(detail_row(pane, depth, "prompt", pane.prompt.trim().to_string()));
    }
    let mut status = format!("status: {}", status_label(&pane.status));
    if !pane.wait_reason.trim().is_empty() {
        status.push_str(&format!(" ({})", pane.wait_reason.trim()));
    }
    out.push(detail_row(pane, depth, "status", status));
    if let Ok(started_at) = pane.started_at.parse::<i64>() {
        let elapsed = (now - started_at).max(0);
        out.push(detail_row(pane, depth, "elapsed", format!("elapsed: {}m{:02}s", elapsed / 60, elapsed % 60)));
    }
    out.push(detail_row(pane, depth, "session", format!("session: {} / pane: {}", pane.session, pane.pane_id)));
    out.push(SidebarRow {
        id: format!("jump::{}", pane.pane_id),
        kind: SidebarRowKind::Jump,
        depth,
        label: "jump".to_string(),
        chat_count: 0,
        rollup: pane.rollup,
        expanded: true,
        pane_id: Some(pane.pane_id.clone()),
        git: None,
        attention: pane.attention,
    });
}
```

## Task 2: input/runtime protocol に filter/reorder/header/click を通す

### RED

`src/sidebar/input.rs` と `src/daemon/runtime.rs` に旧 `handle_key` / daemon apply 相当のテストを移植する。

```rust
#[test]
fn parse_key_maps_view_filter_reorder_and_asymmetric_expand() {
    assert_eq!(parse_key("v"), Some(SidebarInputAction::CycleViewMode));
    assert_eq!(parse_key("tab"), Some(SidebarInputAction::ToggleFilter));
    assert_eq!(parse_key("J"), Some(SidebarInputAction::ReorderDown));
    assert_eq!(parse_key("K"), Some(SidebarInputAction::ReorderUp));
    assert_eq!(parse_key("right"), Some(SidebarInputAction::Expand));
    assert_eq!(parse_key("left"), Some(SidebarInputAction::Collapse));
}

#[test]
fn runtime_skips_detail_rows_when_moving_selection() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let mut pane = pane("%1", "/tmp/app", "codex", "running");
    pane.prompt = "prompt".to_string();
    state.ui_state.toggle_expanded("chat::%1");
    state.apply_event(DaemonEvent::PanesUpdated(vec![pane]));

    state.apply_event(DaemonEvent::Client { client_id: ClientId(1), event: SidebarClientEvent::Key { key: "j".to_string() } });
    state.apply_event(DaemonEvent::Client { client_id: ClientId(1), event: SidebarClientEvent::Key { key: "j".to_string() } });
    state.apply_event(DaemonEvent::Client { client_id: ClientId(1), event: SidebarClientEvent::Key { key: "j".to_string() } });

    assert_eq!(state.ui_state.selection.as_deref(), Some("jump::%1"));
}
```

RED 確認:

```bash
rtk cargo test sidebar::input
rtk cargo test daemon::runtime
```

### GREEN

`SidebarClientEvent` を追加する。

```rust
pub enum SidebarClientEvent {
    Key { key: String },
    JumpPane { pane: String },
    ToggleExpand { row_id: String },
    SetViewMode { view_mode: ViewMode },
    SetFilter { filter: StatusFilter },
}
```

runtime は `Key` と直接 event を同じ reducer に落とす。

```rust
fn apply_client_event(&mut self, event: SidebarClientEvent) -> Vec<RuntimeEffect> {
    match event {
        SidebarClientEvent::Key { key } => self.apply_key(&key),
        SidebarClientEvent::JumpPane { pane } => {
            self.ui_state.selection = Some(format!("chat::{pane}"));
            self.mark_state_dirty(Instant::now());
            self.rebuild_snapshot();
            self.broadcast_if_needed();
            vec![RuntimeEffect::JumpPane(pane)]
        }
        SidebarClientEvent::ToggleExpand { row_id } => self.apply_toggle_row(&row_id),
        SidebarClientEvent::SetViewMode { view_mode } => self.apply_set_view_mode(view_mode),
        SidebarClientEvent::SetFilter { filter } => self.apply_set_filter(filter),
    }
}
```

`row_refs` は `Detail` を除外し、`Jump` は含める。

```rust
pub fn row_refs(rows: &[SidebarRow]) -> Vec<SidebarRowRef> {
    rows.iter()
        .filter(|row| row.kind != SidebarRowKind::Detail)
        .map(|row| SidebarRowRef::new(row.id.clone()))
        .collect()
}
```

## Task 3: render を styled Line/Span 化し、header と色を追加する

### RED

`src/sidebar/render.rs` に TestBackend なしで検査できる style テストを追加する。

```rust
#[test]
fn render_lines_color_rollup_and_category_and_git_badges() {
    let mut repo = row("repo::misc::app", SidebarRowKind::Repo, 0, "app", RollupLevel::Running);
    repo.git = Some(GitBadge { branch: "main".to_string(), ahead: 2, behind: 1 });
    let category = row("category::misc", SidebarRowKind::Category, 0, "misc", RollupLevel::Idle);
    let lines = render_lines(&[category, repo], &SidebarState::default(), 80, &SidebarRenderTheme::default());

    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Blue));
    assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
    assert!(lines[1].spans.iter().any(|span| span.content.as_ref() == "+2" && span.style.fg == Some(Color::Green)));
    assert!(lines[1].spans.iter().any(|span| span.content.as_ref() == "-1" && span.style.fg == Some(Color::Red)));
}

#[test]
fn header_layout_shows_current_values_only_and_hit_tests_tokens() {
    let state = SidebarState { view_mode: ViewMode::ByCategory, filter: StatusFilter::AttentionOnly, ..SidebarState::default() };
    let header = build_header_layout(&state, 80);
    assert_eq!(header.lines[0].text, " category  attn ");
    assert_eq!(header_hit_test(&header, 0, 1), Some(HeaderAction::CycleViewMode));
    assert_eq!(header_hit_test(&header, 0, 11), Some(HeaderAction::ToggleFilter));
}
```

RED 確認:

```bash
rtk cargo test sidebar::render
```

### GREEN

`render_rows` は `--once` 用の文字列シームとして残し、通常 TUI 用に `render_lines` を追加する。

```rust
pub fn render_lines(
    rows: &[SidebarRow],
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Vec<Line<'static>> {
    if width <= 2 {
        return render_rail_lines(rows, state, theme);
    }
    rows.iter().map(|row| render_line(row, state, width, theme)).collect()
}

pub fn render_rows(rows: &[SidebarRow], state: &SidebarState, width: usize) -> String {
    render_lines(rows, state, width, &SidebarRenderTheme::default())
        .into_iter()
        .map(|line| line.spans.into_iter().map(|span| span.content.into_owned()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
```

Header は現在値のみを持つ。

```rust
pub fn build_header_layout(state: &SidebarState, width: u16) -> HeaderLayout {
    if width <= 2 {
        return HeaderLayout::default();
    }
    let mode = match state.view_mode { ViewMode::Flat => "flat", ViewMode::ByRepo => "repo", ViewMode::ByCategory => "category" };
    let filter = match state.filter { StatusFilter::All => "all", StatusFilter::AttentionOnly => "attn" };
    let mut text = format!(" {mode:<8}  {filter:<4} ");
    if text.chars().count() > width as usize {
        text = text.chars().take(width as usize).collect();
    }
    HeaderLayout::from_current_values(text, mode, filter)
}
```

## Task 4: TUI event loop に header hit test / row click / preview を入れる

### RED

`src/sidebar/tui.rs` に offset と preview command の純粋関数テストを追加する。

```rust
#[test]
fn row_for_click_offsets_header_rows() {
    let snapshot = snapshot_with_rows(vec![repo_row(), chat_row("%1")]);
    assert_eq!(row_for_click(&snapshot, 0, 1), Some("repo::misc::app".to_string()));
    assert_eq!(row_for_click(&snapshot, 1, 1), Some("chat::%1".to_string()));
    assert_eq!(row_for_click(&snapshot, 0, 2), None);
}

#[test]
fn preview_command_targets_centered_floating_pane_and_alt_screen() {
    let command = build_preview_command("%26");
    assert_eq!(
        command.args[..12],
        ["new-pane", "-P", "-F", "#{pane_id}", "-x", "80%", "-y", "80%", "-X", "10%", "-Y", "10%"]
    );
    assert!(command.args[12].contains("capture-pane -a -p -e -t '%26'"));
    assert!(command.args[12].contains("capture-pane -p -e -t '%26'"));
}
```

RED 確認:

```bash
rtk cargo test sidebar::tui
```

### GREEN

TUI は受信 snapshot を描画し、header クリックと行クリックを分ける。

```rust
fn handle_mouse_event(
    socket: &Path,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    snapshot: &DaemonSnapshot,
    mouse: MouseEvent,
    clicks: &mut ClickTracker,
) -> Result<()> {
    let width = crossterm::terminal::size().map(|(width, _)| width).unwrap_or(80);
    let header = render::build_header_layout(&snapshot.sidebar.as_ref().unwrap().state, width);
    if mouse.row < header.row_count() {
        match render::header_hit_test(&header, mouse.row, mouse.column) {
            Some(HeaderAction::CycleViewMode) => send_sidebar_key(socket, "v")?,
            Some(HeaderAction::ToggleFilter) => send_sidebar_key(socket, "tab")?,
            None => {}
        }
        return Ok(());
    }

    let Some(row) = row_for_mouse(snapshot, mouse.row, header.row_count()) else {
        return Ok(());
    };
    if clicks.register_left_click(mouse.row, Instant::now()) && row.pane_id.is_some() {
        send_sidebar_jump(socket, row.pane_id.as_ref().unwrap())?;
        return Ok(());
    }
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo | SidebarRowKind::Chat => send_sidebar_toggle(socket, &row.id)?,
        SidebarRowKind::Jump => send_sidebar_jump(socket, row.pane_id.as_ref().unwrap())?,
        SidebarRowKind::Detail => spawn_preview(runner, env, row.pane_id.as_ref().unwrap()),
    }
    Ok(())
}
```

## Task 5: sidebar.colors config と docs/smoke を更新する

### RED

`src/config/mod.rs` と `src/config/schema.rs` に次のテストを追加する。

```rust
#[test]
fn sidebar_colors_accept_old_sidebar_color_keys() {
    let config = serde_yaml_ng::from_str::<Config>(
        "sidebar:\n  colors:\n    running: green\n    selection_bg: \"237\"\n    header_active_bg: \"24\"\n",
    ).unwrap();
    assert_eq!(config.sidebar.colors.running.as_deref(), Some("green"));
    assert_eq!(config.sidebar.colors.selection_bg.as_deref(), Some("237"));
    assert_eq!(config.sidebar.colors.header_active_bg.as_deref(), Some("24"));
}

#[test]
fn schema_contains_sidebar_colors() {
    let schema = config_schema();
    let colors = &schema["properties"]["sidebar"]["properties"]["colors"]["properties"];
    assert_eq!(colors["header_active_bg"]["type"], "string");
    assert_eq!(colors["selection_bg"]["type"], "string");
}

#[test]
fn sidebar_header_style_can_be_configured() {
    let config = serde_yaml_ng::from_str::<Config>(
        "sidebar:\n  header:\n    prefix: \"[\"\n    suffix: \"]\"\n    format: \" {label} \"\n    separator: \" \"\n    bold: true\n    colors:\n      fg: white\n      bg: \"24\"\n",
    ).unwrap();
    assert_eq!(config.sidebar.header.format, " {label} ");
    assert!(config.sidebar.header.bold);
}
```

RED 確認:

```bash
rtk cargo test config
```

### GREEN

`Config.sidebar.colors` を追加する。

```rust
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct SidebarColorsConfig {
    pub error: Option<String>,
    pub running: Option<String>,
    pub permission: Option<String>,
    pub background: Option<String>,
    pub waiting: Option<String>,
    pub idle: Option<String>,
    pub attention: Option<String>,
    pub selection_bg: Option<String>,
    pub selection_active_bg: Option<String>,
    pub header_active_bg: Option<String>,
    pub header_active_fg: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub width: SidebarWidth,
    pub min_width: u16,
    pub colors: SidebarColorsConfig,
    pub header: SidebarHeaderConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SidebarHeaderConfig {
    pub format: String,
    pub prefix: String,
    pub suffix: String,
    pub separator: String,
    pub bold: bool,
    pub colors: SegmentColors,
}
```

README と migration の config 例へ `sidebar.colors` を追加し、`docs/e2e-smoke.md` へ scratch tmux の UI parity 確認ログを追記する。

## 最終品質ゲート

```bash
rtk cargo fmt
rtk cargo test
rtk cargo clippy --all-targets -- -D warnings
rtk cargo fmt --check
rtk cargo build
rtk bash scripts/smoke-m6-runtime.sh
```

scratch tmux 実機確認は `tmux -L <scratch> -f /dev/null` のみで行う。
daemon は `VDE_DAEMON_SOCKET` / `XDG_STATE_HOME` / `XDG_CONFIG_HOME` を隔離し、`target/debug/vt daemon` をシェル子プロセスとして起動する。

## コミット手順

未追跡の別件 plan-11 は add しない。
実装完了後に対象ファイルだけ stage する。

```bash
rtk git add docs/plans/2026-07-04-plan-11-sidebar-tui-ui-parity.md \
  src/sidebar/state.rs src/sidebar/tree.rs src/sidebar/input.rs src/sidebar/render.rs \
  src/sidebar/tui.rs src/sidebar/client.rs src/daemon/protocol.rs src/daemon/runtime.rs \
  src/config/mod.rs src/config/schema.rs README.md docs/migration.md docs/e2e-smoke.md

rtk git commit -m "$(cat <<'EOF'
sidebar TUI の UI パリティを実装する

- Chat 詳細行、filter、reorder、preview、header click を追加
- sidebar.colors と schema/docs を更新
- scratch tmux の UI parity smoke を記録
EOF
)"
```
