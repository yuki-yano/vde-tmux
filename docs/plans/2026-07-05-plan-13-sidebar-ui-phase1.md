# Plan 13: sidebar 表示基盤の刷新(UI 再設計 Phase 1)

> **実装者向け:** 本計画は `docs/sidebar-ui-proposals.md` §9.2 の Phase 1 を実装するもの。Task 順に実施し、各 Task 末尾でテストを通してからコミットする。Task を跨いだ先取り実装はしない。

**Goal:** サイドバーの行レンダリングを「単幅グリフ + 左右 padding + 右端整列カラム + unicode-width truncate + フッター」へ刷新し、後続 Phase(fisheye / TRIAGE / 幅適応)が描画パスを作り直さずに済む構造を作る。

**Architecture:** 行データは daemon 側(`tree.rs`)で構築され NDJSON で client に push、描画は client 側(`render.rs` / `tui.rs`)で行われる。本 Phase では `SidebarRow` に構造化メタ(`RowMeta`)を追加し(wire format 変更・`#[serde(default)]` 付与)、render を「span 合成 + 幅計算」ベースに書き換える。ViewMode / フィルタ / 操作系は一切変更しない。

**Tech Stack:** Rust / ratatui 0.29 / crossterm 0.28 / serde / unicode-width(新規直接依存)

**運用上の注意:** `SidebarRow` の wire format が変わるため、デプロイ後は daemon の再起動が必要(client と daemon は同一バイナリ。旧 daemon + 新 client の混在は想定しない。後方互換対応は行わない方針)。

## DoD

### 機能完了条件

- [x] 既定バッジが `▲`(blocked・赤)/ `●`(working・緑)/ `✓`(done・シアン)/ `○`(idle・DarkGray)の単幅グリフ + 色付き span で表示される
- [x] `badge.glyphs` 設定で絵文字(🔴🟡🔵🟢)に戻せる。`sidebar.colors.badge_*` でバッジ色を上書きできる
- [x] 全行に左右1列の padding が入り、選択行は行頭 `"> "` マーカーなしで背景色 + BOLD がフル幅に塗られる
- [x] Repo/Category 行の `[running:2]` 形式が消え、右端に `▲N`(blocked 件数、0件なら非表示)が右寄せされる
- [x] Chat 行の右端に状態略語(`err`/`perm`/`wait`/`bg`)または running 時の経過時間(`13m`/`45s`)が右寄せされる
- [x] truncate が表示幅(unicode-width)基準で行われ、`…` が付く。CJK を含む prompt で右端カラムが崩れない
- [x] ヘッダーが `" repo · all"` 形式(固定幅パディング廃止)になり、mode/filter のクリック hit-test が機能する(既定 ASCII 設定で機能。CJK 設定は Plan 18 Task 7 で対応)
- [x] 高さ12行以上のときフッター1行(キーヒント、DIM)が表示され、12行未満では自動で消える
- [x] rail(幅≤2)が新グリフで従来どおり動作する
- [x] 幅40列・高さ24行、幅30列、rail で表示崩れがない

### テスト完了条件

- [x] `rtk cargo test` 全通過(既存期待文字列の更新を含む)
- [x] 新規テスト: truncate の CJK 境界、右端カラムの整列、RowMeta 構築、フッターの高さ閾値、選択行のフル幅背景
- [x] `rtk cargo clippy --all-targets` 警告ゼロ
- [x] `rtk cargo fmt --check` 通過

### 運用反映条件

- [x] `docs/e2e-smoke.md` の表示期待(絵文字バッジ・`[running:N]` 表記)を新表示に更新し、scratch tmux で smoke を実施して結果を記録
- [x] `docs/sidebar-ui-proposals.md` §9.2 Phase 1 にチェックを付け、実装中の判断変更があれば追記
- [x] daemon 再起動が必要な旨を commit message または smoke 記録に明記

---

## 表示フォーマット仕様(全 Task 共通の参照)

幅 `W`(>2)の行は次のセル配分に従う。左右 padding 各1、content 幅 = W-2。

```
" " + head + [badge] + label(truncate対象) + [git] + filler + [right] + " "
```

| 行種 | head | badge | right |
|---|---|---|---|
| Category / Repo | `{indent}{▾|▸} ` | なし | `▲N`(meta.attention_count>0 時のみ、badge_blocked 色) |
| Chat | `{indent}{▾|▸} ` | `{glyph} `(badge_color 色) | rollup 由来: Error→`err` / Permission→`perm` / Waiting→`wait` / Background→`bg` / Running→経過(`45s`/`13m`)/ Idle→なし(DIM + rollup 色) |
| Detail | `{indent}` | なし | なし |
| Jump | `{indent}-> ` | なし | なし |

- indent は `"  "` × depth(従来どおり)
- 展開マーカーは `v`/`>` → `▾`/`▸`
- git バッジ(Repo のみ)は label 直後に ` main +2 -1`(従来の色分けを維持)
- label は「content 幅 − head − badge − git − (right幅+1)」まで。超過時は `…` 付き truncate
- filler は右カラムを右端(右 padding の内側)に揃えるための空白
- 選択行: 行頭マーカーなし。Line 全体に `bg(selection_bg) + BOLD`、filler で行末まで埋めて背景をフル幅化
- ヘッダー: `" {mode} · {filter}"`(既定。`sidebar.header` の prefix/suffix/format はそのまま機能、`{label:<width$}` の固定幅パディングのみ廃止)
- フッター: `" j/k move  enter jump  tab filter"`(DIM)。幅≤2 または 高さ<12 で非表示

表示例(幅40、repo 展開、chat 選択・running 13分・blocked 1件が同居):

```
 repo · all
 ▾ vde-tmux main +2                  ▲1
   ▸ ● claude: fix sidebar flick…  13m
   ▸ ▲ codex: review PR #42       perm
 j/k move  enter jump  tab filter
```

---

## Task 0: unicode-width 依存と幅ヘルパ

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/sidebar/render.rs`(ヘルパ追加のみ。既存関数は触らない)

- [ ] **Step 1: Cargo.toml に依存追加**

`Cargo.toml` の `[dependencies]` に追加(ratatui 0.29 が使う 0.2 系に合わせる):

```toml
unicode-width = "0.2"
```

- [ ] **Step 2: 失敗するテストを書く**

`src/sidebar/render.rs` の `#[cfg(test)] mod tests` 末尾に追加:

```rust
#[test]
fn display_width_counts_cjk_as_two_cells() {
    assert_eq!(display_width("abc"), 3);
    assert_eq!(display_width("あいう"), 6);
    assert_eq!(display_width("a…"), 2);
}

#[test]
fn truncate_display_appends_ellipsis_within_width() {
    assert_eq!(truncate_display("hello", 10), "hello");
    assert_eq!(truncate_display("hello world", 8), "hello w…");
    // CJK: 幅7に「あいうえお」(幅10)→ 幅6以内で切って … を足す(合計幅7以内)
    assert_eq!(truncate_display("あいうえお", 7), "あいう…");
    assert_eq!(truncate_display("abc", 0), "");
}
```

- [ ] **Step 3: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render::tests::display_width_counts_cjk_as_two_cells`
Expected: コンパイルエラー(`display_width` 未定義)

- [ ] **Step 4: 実装**

`src/sidebar/render.rs` の `truncate_width`(399-404行)の直後に追加(`truncate_width` はまだ消さない。Task 3 で置き換える):

```rust
pub(crate) fn display_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

pub(crate) fn truncate_display(text: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_string();
    }
    let budget = max_width - 1; // "…"(幅1)の分を確保
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > budget {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}
```

- [ ] **Step 5: テスト通過を確認**

Run: `rtk cargo test --lib sidebar::render::tests`
Expected: PASS(既存テスト含め全通過)

- [ ] **Step 6: コミット**

```bash
rtk git add Cargo.toml Cargo.lock src/sidebar/render.rs
rtk git commit -m "sidebar に unicode-width ベースの幅ヘルパを追加する"
```

---

## Task 1: バッジグリフの既定値刷新とバッジ色

**Files:**
- Modify: `src/config/mod.rs:167-176`(BadgeGlyphs::default)、`src/config/mod.rs:214-226`(SidebarColorsConfig)
- Modify: `src/sidebar/render.rs`(SidebarRenderTheme に badge 色を追加)
- Modify: 既存テストの絵文字期待値(render.rs / statusline や session_badge 側で絵文字既定値に依存するテストがあれば同様に更新)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests に追加:

```rust
#[test]
fn theme_maps_badge_states_to_default_colors() {
    let theme = SidebarRenderTheme::default();
    assert_eq!(theme.badge_color(BadgeState::Blocked), Color::Red);
    assert_eq!(theme.badge_color(BadgeState::Working), Color::Green);
    assert_eq!(theme.badge_color(BadgeState::Done), Color::Cyan);
    assert_eq!(theme.badge_color(BadgeState::Idle), Color::DarkGray);
}

#[test]
fn badge_colors_are_configurable() {
    let config = serde_yaml_ng::from_str::<crate::config::Config>(
        r##"
sidebar:
  colors:
    badge_working: yellow
"##,
    )
    .unwrap();
    let theme = SidebarRenderTheme::from_sidebar_config(&config.sidebar);
    assert_eq!(theme.badge_color(BadgeState::Working), Color::Yellow);
    assert_eq!(theme.badge_color(BadgeState::Blocked), Color::Red);
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render::tests::theme_maps_badge_states_to_default_colors`
Expected: コンパイルエラー(`badge_color` 未定義)

- [ ] **Step 3: 実装 — config**

`src/config/mod.rs` の `BadgeGlyphs::default`(167-176行)を変更:

```rust
impl Default for BadgeGlyphs {
    fn default() -> Self {
        Self {
            blocked: "▲".to_string(),
            working: "●".to_string(),
            done: "✓".to_string(),
            idle: "○".to_string(),
        }
    }
}
```

`SidebarColorsConfig`(214-226行)にフィールド追加:

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
    pub badge_blocked: Option<String>,
    pub badge_working: Option<String>,
    pub badge_done: Option<String>,
    pub badge_idle: Option<String>,
}
```

- [ ] **Step 4: 実装 — theme**

`src/sidebar/render.rs` の `SidebarRenderTheme`(8-25行)にフィールド追加:

```rust
    pub badge_blocked: Color,
    pub badge_working: Color,
    pub badge_done: Color,
    pub badge_idle: Color,
```

`Default`(27-47行)に追加:

```rust
            badge_blocked: Color::Red,
            badge_working: Color::Green,
            badge_done: Color::Cyan,
            badge_idle: Color::DarkGray,
```

`from_config`(49-70行)に追加:

```rust
            badge_blocked: parse_color(config.badge_blocked.as_deref())
                .unwrap_or(default.badge_blocked),
            badge_working: parse_color(config.badge_working.as_deref())
                .unwrap_or(default.badge_working),
            badge_done: parse_color(config.badge_done.as_deref()).unwrap_or(default.badge_done),
            badge_idle: parse_color(config.badge_idle.as_deref()).unwrap_or(default.badge_idle),
```

`impl SidebarRenderTheme`(92-106行)にメソッド追加:

```rust
    pub(crate) fn badge_color(&self, state: BadgeState) -> Color {
        match state {
            BadgeState::Blocked => self.badge_blocked,
            BadgeState::Working => self.badge_working,
            BadgeState::Done => self.badge_done,
            BadgeState::Idle => self.badge_idle,
        }
    }
```

- [ ] **Step 5: 既存テストの絵文字期待値を更新**

`src/sidebar/render.rs`:
- `render_rows_uses_rail_for_narrow_width`(549行): `assert_eq!(rendered, "🔴");` → `assert_eq!(rendered, "▲");`
- `chat_rows_render_badge_glyph_and_omit_trailing_status_text`(756行): `rendered.contains("🟡 codex (%1)")` → `rendered.contains("● codex (%1)")`
- `rail_uses_badge_glyphs`(773行): `assert_eq!(rendered, "🔵");` → `assert_eq!(rendered, "✓");`

さらに `rtk cargo test` を全体実行し、絵文字既定値(🔴🟡🔵🟢)に依存して失敗するテストが sidebar 以外(statusline / session_badge / daemon)にあれば、同じ対応表(🔴→▲ / 🟡→● / 🔵→✓ / 🟢→○)で期待値を更新する。ロジックは変えない。

- [ ] **Step 6: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過

- [ ] **Step 7: コミット**

```bash
rtk git add -A
rtk git commit -m "badge glyph の既定値を単幅グリフに変更しバッジ色を設定可能にする"
```

---

## Task 2: RowMeta の追加と tree 構築

**Files:**
- Modify: `src/sidebar/tree.rs`(RowMeta 定義、SidebarRow.meta、構築5箇所)
- Modify: `src/sidebar/render.rs` tests の `row()` ヘルパ(506-518行)に `meta: None` 追加

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/tree.rs` の `#[cfg(test)] mod tests` に追加。既存テストのスナップショット生成ヘルパ(`pane(...)` 等。tree.rs 559行以降の既存テストが使っているものをそのまま使う)に倣って書く:

```rust
#[test]
fn chat_rows_carry_row_meta() {
    // 既存テストと同じ流儀で PaneSnapshot を1つ作る:
    // agent="codex", prompt="fix bug", status="running",
    // started_at=(now-75).to_string(), tasks="2/5",
    // subagents="sub1:Explore|ab12:general-purpose"
    // build_rows_at で now を固定して構築する
    let rows = /* build_rows_at(...) を ViewMode::Flat で呼ぶ */;
    let chat = rows
        .iter()
        .find(|row| row.kind == SidebarRowKind::Chat)
        .expect("chat row");
    let meta = chat.meta.as_ref().expect("chat meta");
    assert_eq!(meta.agent.as_deref(), Some("codex"));
    assert_eq!(meta.prompt.as_deref(), Some("fix bug"));
    assert_eq!(meta.elapsed_secs, Some(75));
    assert_eq!(meta.tasks_done, Some(2));
    assert_eq!(meta.tasks_total, Some(5));
    assert_eq!(meta.subagent_count, Some(2));
}

#[test]
fn repo_rows_carry_blocked_count_in_meta() {
    // blocked(permission 待ち)1体 + running 1体を同一 repo に置いて ByRepo で構築
    let rows = /* build_rows_at(...) */;
    let repo = rows
        .iter()
        .find(|row| row.kind == SidebarRowKind::Repo)
        .expect("repo row");
    assert_eq!(
        repo.meta.as_ref().and_then(|meta| meta.attention_count),
        Some(1)
    );
}
```

(コメント部分は既存テストの PaneSnapshot 構築ヘルパを流用して具体化する。tree.rs の既存テストに `PaneSnapshot` を組み立てるコードが必ずあるので、そのパターンをコピーする。)

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::tree::tests::chat_rows_carry_row_meta`
Expected: コンパイルエラー(`meta` フィールド未定義)

- [ ] **Step 3: 実装 — 型定義**

`src/sidebar/tree.rs` の `SidebarRow`(22-34行)にフィールド追加と `RowMeta` 定義:

```rust
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
    #[serde(default)]
    pub meta: Option<RowMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RowMeta {
    pub agent: Option<String>,
    pub prompt: Option<String>,
    pub wait_reason: Option<String>,
    pub elapsed_secs: Option<i64>,
    pub tasks_done: Option<i64>,
    pub tasks_total: Option<i64>,
    pub subagent_count: Option<usize>,
    pub attention_count: Option<usize>,
}
```

- [ ] **Step 4: 実装 — 構築**

ヘルパを追加:

```rust
fn chat_meta(pane: &AgentPane, now: i64) -> RowMeta {
    let tasks = parse_tasks(&pane.tasks);
    RowMeta {
        agent: Some(pane.agent.clone()),
        prompt: non_empty(&pane.prompt).map(str::to_string),
        wait_reason: non_empty(&pane.wait_reason).map(str::to_string),
        elapsed_secs: pane
            .started_at
            .parse::<i64>()
            .ok()
            .map(|started_at| (now - started_at).max(0)),
        tasks_done: tasks.map(|(done, _)| done),
        tasks_total: tasks.map(|(_, total)| total),
        subagent_count: Some(decode_subagents(&pane.subagents).len()),
        attention_count: None,
    }
}

fn group_meta(panes: &[AgentPane]) -> RowMeta {
    RowMeta {
        attention_count: Some(
            panes
                .iter()
                .filter(|pane| pane.badge_state == BadgeState::Blocked)
                .count(),
        ),
        ..RowMeta::default()
    }
}
```

`SidebarRow` を構築している5箇所を更新:
- category 行(tree.rs:184-195): `meta: Some(group_meta(&all_panes)),`
- repo 行(tree.rs:253-264): `meta: Some(group_meta(&panes)),`
- chat 行(`push_chat_row`, tree.rs:295-306): `meta: Some(chat_meta(pane, now)),`
- `detail_row`(tree.rs:312-325): `meta: None,`
- jump 行(tree.rs:367-378): `meta: None,`

`src/sidebar/render.rs` tests の `row()` ヘルパ(506-518行)と `src/sidebar/input.rs` tests の `row()` ヘルパ(72-85行)に `meta: None,` を追加。他に `SidebarRow { .. }` をリテラル構築している箇所があればコンパイルエラーで検出されるので、同様に `meta: None` を足す。

**push fingerprint の更新(必須)**: `src/daemon/runtime.rs` の `current_fingerprint`(235-249行)は `id/label/chat_count/rollup/badge_state/git` しか見ておらず、`meta` の変化(elapsed_secs が毎秒変わる等)では push されない。format 文字列に `{:?}` で `row.meta` を追加する:

```rust
                    format!(
                        "{}:{}:{}:{:?}:{:?}:{:?}:{:?}",
                        row.id,
                        row.label,
                        row.chat_count,
                        row.rollup,
                        row.badge_state,
                        row.git,
                        row.meta
                    )
```

これにより elapsed_secs の変化で毎ポーリング(既定1秒)push が発生する。ローカル Unix socket 上の小さな NDJSON であり許容範囲(結果として経過時間表示が毎秒 live 更新される)。この意図を commit message に明記する。

- [ ] **Step 5: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過

- [ ] **Step 6: コミット**

```bash
rtk git add -A
rtk git commit -m "SidebarRow に構造化メタ RowMeta を追加する"
```

---

## Task 3: 行レンダリングの刷新(padding・右端カラム・選択フル幅)

**Files:**
- Modify: `src/sidebar/render.rs`(`render_row_line` / `render_row_text` / `render_repo_line_with_git` / `truncate_width` を置き換え)
- Modify: 同ファイルの既存テスト期待値

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests に追加:

```rust
#[test]
fn rows_have_horizontal_padding_and_no_selection_marker() {
    let rows = vec![row(
        "repo::misc::app",
        SidebarRowKind::Repo,
        0,
        "app",
        RollupLevel::Running,
    )];
    let state = SidebarState {
        selection: Some("repo::misc::app".to_string()),
        ..SidebarState::default()
    };
    let rendered = render_rows(&rows, &state, 20);
    // 左 padding 1 + マーカー。"> " は付かない。行は幅いっぱいまで空白で埋まる
    assert!(rendered.starts_with(" ▾ app"), "{rendered:?}");
    assert!(!rendered.contains("> "), "{rendered:?}");
    assert_eq!(display_width(&rendered), 20, "{rendered:?}");
}

#[test]
fn repo_row_right_aligns_attention_count() {
    let mut repo = row(
        "repo::misc::app",
        SidebarRowKind::Repo,
        0,
        "app",
        RollupLevel::Permission,
    );
    repo.meta = Some(crate::sidebar::tree::RowMeta {
        attention_count: Some(2),
        ..Default::default()
    });
    let rendered = render_rows(&[repo], &SidebarState::default(), 20);
    // "▲2" が右端(右 padding の内側)に来る
    assert!(rendered.ends_with("▲2 "), "{rendered:?}");
    assert!(!rendered.contains("[permission:"), "{rendered:?}");
}

#[test]
fn chat_row_right_aligns_status_short_label() {
    let mut chat = row(
        "chat::%1",
        SidebarRowKind::Chat,
        0,
        "codex: review PR",
        RollupLevel::Permission,
    );
    chat.badge_state = Some(BadgeState::Blocked);
    let rendered = render_rows(&[chat], &SidebarState::default(), 30);
    assert!(rendered.ends_with("perm "), "{rendered:?}");
    assert!(rendered.contains("▲ codex: review PR"), "{rendered:?}");
}

#[test]
fn chat_row_shows_elapsed_when_running() {
    let mut chat = row(
        "chat::%1",
        SidebarRowKind::Chat,
        0,
        "codex: fix",
        RollupLevel::Running,
    );
    chat.badge_state = Some(BadgeState::Working);
    chat.meta = Some(crate::sidebar::tree::RowMeta {
        elapsed_secs: Some(815),
        ..Default::default()
    });
    let rendered = render_rows(&[chat], &SidebarState::default(), 30);
    assert!(rendered.ends_with("13m "), "{rendered:?}");
}

#[test]
fn long_cjk_label_is_truncated_with_ellipsis_keeping_right_column() {
    let mut chat = row(
        "chat::%1",
        SidebarRowKind::Chat,
        0,
        "codex: 日本語のとても長いプロンプトを表示する",
        RollupLevel::Permission,
    );
    chat.badge_state = Some(BadgeState::Blocked);
    let rendered = render_rows(&[chat], &SidebarState::default(), 24);
    assert!(rendered.contains('…'), "{rendered:?}");
    assert!(rendered.ends_with("perm "), "{rendered:?}");
    assert_eq!(display_width(&rendered), 24, "{rendered:?}");
}

#[test]
fn badge_glyph_is_rendered_in_badge_color_span() {
    let mut chat = row(
        "chat::%1",
        SidebarRowKind::Chat,
        0,
        "codex",
        RollupLevel::Running,
    );
    chat.badge_state = Some(BadgeState::Working);
    let lines = render_lines(
        &[chat],
        &SidebarState::default(),
        30,
        &SidebarRenderTheme::default(),
    );
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|span| span.content.contains('●') && span.style.fg == Some(Color::Green)),
        "{lines:?}"
    );
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render::tests::rows_have_horizontal_padding_and_no_selection_marker`
Expected: FAIL(現行は `"  v app [running:1]"` 形式)

- [ ] **Step 3: 実装**

`src/sidebar/render.rs` の `render_row_line`(267-285行)・`render_row_text`(287-325行)・`render_repo_line_with_git`(327-352行)・`truncate_width`(399-404行)を削除し、以下に置き換える。`GitBadgeText` / `format_git_badge_parts`(354-368行)は残す:

```rust
fn render_row_line(
    row: &SidebarRow,
    state: &SidebarState,
    width: usize,
    theme: &SidebarRenderTheme,
) -> Line<'static> {
    let selected = state.selection.as_deref() == Some(row.id.as_str());
    let style = row_style(row, theme);
    let content_width = width.saturating_sub(2);

    let indent = "  ".repeat(row.depth);
    let head = match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo | SidebarRowKind::Chat => {
            let marker = if row.expanded { "▾" } else { "▸" };
            format!("{indent}{marker} ")
        }
        SidebarRowKind::Detail => indent.clone(),
        SidebarRowKind::Jump => format!("{indent}-> "),
    };
    let badge = if row.kind == SidebarRowKind::Chat {
        row.badge_state.map(|state| {
            (
                format!("{} ", theme.badge_glyph(state)),
                theme.badge_color(state),
            )
        })
    } else {
        None
    };
    let git = if row.kind == SidebarRowKind::Repo {
        row.git
            .as_ref()
            .map(format_git_badge_parts)
            .filter(|git| !git.branch.is_empty())
    } else {
        None
    };
    let right = right_label(row);

    let badge_width = badge
        .as_ref()
        .map(|(text, _)| display_width(text))
        .unwrap_or(0);
    let git_width = git.as_ref().map(git_badge_width).unwrap_or(0);
    let right_width = right.as_deref().map(display_width).unwrap_or(0);
    let right_reserved = if right_width > 0 { right_width + 1 } else { 0 };
    let label_budget = content_width
        .saturating_sub(display_width(&head))
        .saturating_sub(badge_width)
        .saturating_sub(git_width)
        .saturating_sub(right_reserved);
    let label = truncate_display(&row.label, label_budget);

    let mut spans = vec![Span::styled(format!(" {head}"), style)];
    if let Some((glyph, color)) = badge {
        spans.push(Span::styled(glyph, Style::default().fg(color)));
    }
    spans.push(Span::styled(label, style));
    if let Some(git) = &git {
        spans.push(Span::styled(format!(" {}", git.branch), style));
        if let Some(ahead) = &git.ahead {
            spans.push(Span::styled(format!(" {ahead}"), style.fg(Color::Green)));
        }
        if let Some(behind) = &git.behind {
            spans.push(Span::styled(format!(" {behind}"), style.fg(Color::Red)));
        }
    }
    let used: usize = spans
        .iter()
        .map(|span| display_width(&span.content))
        .sum();
    let filler = width
        .saturating_sub(1)
        .saturating_sub(used)
        .saturating_sub(right_width);
    spans.push(Span::raw(" ".repeat(filler)));
    if let Some(right) = right {
        spans.push(Span::styled(right, right_style(row, theme)));
    }
    spans.push(Span::raw(" "));

    let mut line = Line::from(spans);
    if selected {
        line = line.style(
            Style::default()
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD),
        );
    }
    line
}

fn right_label(row: &SidebarRow) -> Option<String> {
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            let count = row.meta.as_ref()?.attention_count?;
            (count > 0).then(|| format!("▲{count}"))
        }
        SidebarRowKind::Chat => match row.rollup {
            RollupLevel::Error => Some("err".to_string()),
            RollupLevel::Permission => Some("perm".to_string()),
            RollupLevel::Waiting => Some("wait".to_string()),
            RollupLevel::Background => Some("bg".to_string()),
            RollupLevel::Running => row
                .meta
                .as_ref()
                .and_then(|meta| meta.elapsed_secs)
                .map(elapsed_label),
            RollupLevel::Idle => None,
        },
        SidebarRowKind::Detail | SidebarRowKind::Jump => None,
    }
}

fn elapsed_label(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

fn right_style(row: &SidebarRow, theme: &SidebarRenderTheme) -> Style {
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo => {
            Style::default().fg(theme.badge_color(BadgeState::Blocked))
        }
        _ => Style::default()
            .fg(theme.rollup_color(row.rollup))
            .add_modifier(Modifier::DIM),
    }
}

fn git_badge_width(git: &GitBadgeText) -> usize {
    let mut width = 1 + display_width(&git.branch);
    if let Some(ahead) = &git.ahead {
        width += 1 + display_width(ahead);
    }
    if let Some(behind) = &git.behind {
        width += 1 + display_width(behind);
    }
    width
}
```

`build_header_layout_with_theme` 内の `truncate_width` 呼び出し(157行)は `truncate_display` に差し替える(ヘッダー刷新自体は Task 4)。

- [ ] **Step 4: 既存テストの期待値を更新**

`src/sidebar/render.rs`:
- `render_rows_includes_selection_indentation_and_rollup`(521行): `"v app [running:1]"` → `" ▾ app"` を contains で検証、`">   v codex %1"` → `"   ▾ codex %1"` を contains で検証(選択は文字でなく背景なので、`!rendered.contains("> ")` も追加)
- `render_repo_row_includes_git_badge`(564行): 変更なしで通るはず(`"main +2 -1"` は残る)。通らなければ期待値を実出力に合わせて確認
- `render_repo_row_omits_zero_git_counts`(584行): `"v app [idle:1] main"` → `"▾ app main"` を contains で検証
- `render_lines_color_rollup_category_selection_and_git_badges`(606行): 選択行の bg 検証は `lines[1].style.bg == Some(Color::Indexed(237))`(span でなく Line 側の style)に変更。`+2`/`-1` の span 検証はそのまま
- `chat_rows_render_badge_glyph_and_omit_trailing_status_text`(756行): `"● codex (%1)"` contains のまま(Task 1 で更新済み)。加えて `!rendered.contains("[running]")` はそのまま

`src/sidebar/tui.rs` のテスト(`renders_snapshot_rows_on_push`, 540行)は `"codex (%1)"` の contains 検証なので影響しないはずだが、`rtk cargo test` で確認し、失敗する場合は本仕様の表示例に沿って期待値を更新する。`src/cli/tests/sidebar.rs` の attach --once 系も同様(表示仕様表から導出できる)。

- [ ] **Step 5: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過

- [ ] **Step 6: コミット**

```bash
rtk git add -A
rtk git commit -m "sidebar 行レンダリングを padding と右端カラム付きに刷新する"
```

---

## Task 4: ヘッダーの固定幅パディング廃止

**Files:**
- Modify: `src/sidebar/render.rs:137-186`(定数削除、`build_header_layout_with_theme`、`format_header_segment`)
- Modify: `src/config/mod.rs:239-250`(SidebarHeaderConfig::default の format)
- Modify: ヘッダー既存テスト3件

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests の `header_layout_defaults_to_statusline_category_like_segments`(682行)を次の内容に書き換える(旧アサーションは削除):

```rust
#[test]
fn header_layout_defaults_to_compact_dot_separated_segments() {
    let state = SidebarState {
        view_mode: ViewMode::ByRepo,
        filter: StatusFilter::All,
        ..SidebarState::default()
    };

    let header = build_header_layout(&state, 80);

    assert_eq!(header.lines[0].text, " repo · all");
    assert_eq!(header.lines[0].segments[0].range, 1..5);
    assert_eq!(header.lines[0].segments[1].range, 8..11);
    assert_eq!(
        header_hit_test(&header, 0, 2),
        Some(HeaderAction::CycleViewMode)
    );
    assert_eq!(
        header_hit_test(&header, 0, 9),
        Some(HeaderAction::ToggleFilter)
    );
    assert_eq!(header_hit_test(&header, 0, 6), None);
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render::tests::header_layout_defaults_to_compact_dot_separated_segments`
Expected: FAIL(現行は `"repo     all       "`)

- [ ] **Step 3: 実装**

`src/config/mod.rs` の `SidebarHeaderConfig::default`(239-250行): `format: "{label} ".to_string()` → `format: "{label}".to_string()`。

`src/sidebar/render.rs`:
- 定数 `VIEW_MODE_BADGE_WIDTH` / `FILTER_BADGE_WIDTH`(137-138行)を削除
- `SidebarRenderTheme::default` の `header_format`(40行)を `"{label}".to_string()` に変更
- `format_header_segment`(182-186行)を置き換え:

```rust
fn format_header_segment(label: &str, theme: &SidebarRenderTheme) -> String {
    let body = theme.header_format.replace("{label}", label);
    format!("{}{}{}", theme.header_prefix, body, theme.header_suffix)
}
```

- `build_header_layout_with_theme`(144-180行)を置き換え:

```rust
pub fn build_header_layout_with_theme(
    state: &SidebarState,
    width: u16,
    theme: &SidebarRenderTheme,
) -> HeaderLayout {
    if width <= 2 {
        return HeaderLayout::default();
    }
    let mode_badge = format_header_segment(view_mode_label(state.view_mode), theme);
    let filter_badge = format_header_segment(filter_label(state.filter), theme);
    let separator = if theme.header_separator.is_empty() {
        " · ".to_string()
    } else {
        theme.header_separator.clone()
    };
    let full_text = format!(" {mode_badge}{separator}{filter_badge}");
    let text = truncate_display(&full_text, width as usize);
    let mut segments = Vec::new();
    let mode_len = mode_badge.chars().count();
    let separator_len = separator.chars().count();
    if let Some(range) = visible_segment_range(&text, 1, mode_len) {
        segments.push(HeaderSegment {
            range,
            action: HeaderAction::CycleViewMode,
        });
    }
    if let Some(range) = visible_segment_range(
        &text,
        1 + mode_len + separator_len,
        filter_badge.chars().count(),
    ) {
        segments.push(HeaderSegment {
            range,
            action: HeaderAction::ToggleFilter,
        });
    }
    HeaderLayout {
        lines: vec![HeaderLine { text, segments }],
    }
}
```

- [ ] **Step 4: 既存テストの期待値を更新**

- `header_layout_shows_current_values_only_and_hit_tests_tokens`(661行): 期待テキスト `" category · attention"`、hit-test は column 2 → CycleViewMode、column 13 → ToggleFilter に変更
- `header_layout_can_be_configured_as_pill_buttons`(716行): separator " " / format " {label} " / prefix "[" / suffix "]" の設定なので期待テキスト `" [ repo ] [ all ]"`、segments は `1..9` と `10..17`。span の色検証はそのまま

- [ ] **Step 5: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過

- [ ] **Step 6: コミット**

```bash
rtk git add -A
rtk git commit -m "sidebar header の固定幅パディングを廃止し dot 区切りにする"
```

---

## Task 5: フッターとレイアウト分割・クリック境界

**Files:**
- Modify: `src/sidebar/render.rs`(`build_footer_line` 追加)
- Modify: `src/sidebar/tui.rs`(`compute_areas` 追加、`draw_snapshot_in_area` 339-376行、`handle_left_click` 392-427行)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/tui.rs` tests に追加(既存の `renders_snapshot_rows_on_push` が使う TestBackend のパターンを流用):

```rust
#[test]
fn footer_is_rendered_when_height_is_sufficient() {
    // TestBackend: 幅40 高さ24。既存テストと同様に snapshot を push して描画
    // バッファ最下行に "j/k move" が含まれることを検証
}

#[test]
fn footer_is_hidden_when_height_is_small() {
    // TestBackend: 幅40 高さ8。バッファに "j/k move" が含まれないことを検証
}

#[test]
fn clicks_below_visible_rows_are_ignored() {
    // rows 2件、幅40 高さ24 の compute_areas で
    // header_rows + rows_height 以上の y を row_for_click 相当の境界判定に通し
    // None になることを検証(compute_areas を直接ユニットテスト)
    let header = HeaderLayout {
        lines: vec![HeaderLine {
            text: " repo · all".to_string(),
            segments: Vec::new(),
        }],
    };
    let areas = compute_areas(Rect::new(0, 0, 40, 24), &header);
    assert_eq!(areas.header_rows, 1);
    assert_eq!(areas.footer_rows, 1);
    assert_eq!(areas.rows_height, 22);

    let small = compute_areas(Rect::new(0, 0, 40, 8), &header);
    assert_eq!(small.footer_rows, 0);
    assert_eq!(small.rows_height, 7);
}
```

(バッファ検証2件は既存 `renders_snapshot_rows_on_push`(tui.rs:540)の TestBackend + buffer 文字列化のコードをコピーして具体化する。)

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::tui::tests::clicks_below_visible_rows_are_ignored`
Expected: コンパイルエラー(`compute_areas` 未定義)

- [ ] **Step 3: 実装 — footer(render.rs)**

```rust
pub fn build_footer_line(width: usize) -> Line<'static> {
    let text = truncate_display(" j/k move  enter jump  tab filter", width);
    Line::from(Span::styled(
        text,
        Style::default().add_modifier(Modifier::DIM),
    ))
}
```

- [ ] **Step 4: 実装 — レイアウト(tui.rs)**

```rust
pub(crate) struct SidebarAreas {
    pub(crate) header_rows: u16,
    pub(crate) rows_height: u16,
    pub(crate) footer_rows: u16,
}

pub(crate) fn compute_areas(area: Rect, header: &HeaderLayout) -> SidebarAreas {
    let header_rows = header.row_count().min(area.height);
    let remaining = area.height.saturating_sub(header_rows);
    let footer_rows = if area.width > 2 && area.height >= 12 && remaining > 1 {
        1
    } else {
        0
    };
    SidebarAreas {
        header_rows,
        rows_height: remaining.saturating_sub(footer_rows),
        footer_rows,
    }
}
```

`draw_snapshot_in_area`(339-376行)の header 描画後を置き換え:

```rust
    let areas = compute_areas(area, &header);
    if areas.header_rows > 0 {
        let header_area = Rect {
            height: areas.header_rows,
            ..area
        };
        frame.render_widget(
            Paragraph::new(render_header_lines(&header, theme)),
            header_area,
        );
    }
    let rows_area = Rect {
        y: area.y + areas.header_rows,
        height: areas.rows_height,
        ..area
    };
    let items = render_lines(&sidebar.rows, &sidebar.state, area.width as usize, theme)
        .into_iter()
        .map(ListItem::new)
        .collect::<Vec<_>>();
    let list = List::new(items).block(Block::default().borders(Borders::NONE));
    frame.render_widget(list, rows_area);
    if areas.footer_rows > 0 {
        let footer_area = Rect {
            y: area.y + areas.header_rows + areas.rows_height,
            height: areas.footer_rows,
            ..area
        };
        frame.render_widget(
            Paragraph::new(build_footer_line(area.width as usize)),
            footer_area,
        );
    }
```

(`build_footer_line` を `use` に追加。)

`handle_left_click`(392-427行)の header 判定後、`row_for_click` の前に境界チェックを追加。`crossterm::terminal::size()` から高さも取る:

```rust
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    let header = build_header_layout_with_theme(&sidebar.state, width, context.theme);
    // ...(header hit-test は従来どおり)...
    let areas = compute_areas(Rect::new(0, 0, width, height), &header);
    if row >= areas.header_rows + areas.rows_height {
        return Ok(());
    }
```

- [ ] **Step 5: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過

- [ ] **Step 6: コミット**

```bash
rtk git add -A
rtk git commit -m "sidebar にフッターを追加しクリック境界をレイアウトに揃える"
```

---

## Task 6: smoke・ドキュメント・品質ゲート

**Files:**
- Modify: `docs/e2e-smoke.md`(バッジ・行表示の期待値)
- Modify: `docs/sidebar-ui-proposals.md`(§9.2 Phase 1 チェック)

- [ ] **Step 1: 品質ゲート**

Run:
```bash
rtk cargo fmt --check
rtk cargo clippy --all-targets
rtk cargo test
```
Expected: すべて警告・エラーなし

- [ ] **Step 2: docs/e2e-smoke.md の更新**

絵文字バッジ(🔴🟡🔵🟢)と `[running:N]` 表記に言及している手順を、本計画の「表示フォーマット仕様」の表示例(`▲`/`●`/`✓`/`○`、右端カラム、フッター)に合わせて書き換える。

- [ ] **Step 3: scratch tmux で smoke 実施**

`docs/e2e-smoke.md` の手順に従い scratch tmux セッションで確認する。最低限:
- 幅40: ヘッダー `" repo · all"`、行の padding、右端カラム、フッターの表示
- `Tab` で `" repo · attention"` に切り替わり、blocked のみ残る
- 幅を狭めて rail(≤2列)がグリフ表示で動く
- 高さを詰めてフッターが消える
- daemon 再起動後に新旧 client が混在しないこと(旧 daemon が残っていた場合は `SidebarRow` のデシリアライズで snapshot が欠けるため、必ず daemon を再起動する)

結果を `docs/e2e-smoke.md` の記録欄(既存の記録形式に合わせる)へ追記する。

- [ ] **Step 4: 提案ドキュメントの更新**

`docs/sidebar-ui-proposals.md` §9.2 の Phase 1 項目にチェックを付け、実装中に仕様から逸れた判断があれば §9.1 の様式で追記する。

- [ ] **Step 5: コミット**

```bash
rtk git add docs/e2e-smoke.md docs/sidebar-ui-proposals.md
rtk git commit -m "Plan 13 の smoke 結果と docs を更新する"
```

---

## 本計画のスコープ外(次 Phase へ)

- 選択行 inline meta・`n/N` 巡回・jump & return・unread 即時既読化 → Phase 2(別プラン)
- TRIAGE 常設ゾーン → Phase 3(別プラン)
- pin・可変行高・幅ティア dense/micro・rail 2部構成 → Phase 4(Phase 1 では `render_row_line` を関数として独立させたことが下準備に当たる。幅ティア引数の導入は Phase 4 で行う)
- フィルタバー多値化・LIVE ペイン → Phase 5

## 実装ノート

- scratch tmux の TUI pane は `capture-pane -a` で alt-screen 文字列を取得できなかったため、表示 smoke は `vt sidebar attach --once` で width40 の padding / 右端カラム / `[running:N]` 非表示と width2 rail glyph を確認した。
- header / footer / 高さ閾値は同じ Task の `sidebar::tui` TestBackend テストで確認し、scratch daemon は明示 socket で再起動して `SidebarRow.meta.attention_count` を含む snapshot wire format を確認した。
