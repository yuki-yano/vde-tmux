# Plan 23: サイドバー視認性ポリッシュ 6項目

> **実装者向け:** `docs/sidebar-improvements-proposals.html` の推奨案(全6項目)の実装。**Plan 13〜22 完了が前提**。Task 順(軽い順・依存順)に実施する。各 Task は TDD で進め、Task ごとにコミットする。

**Goal:** pin 強調、mode/category/repo の3役色分け、category 区切り線、LIVE のカード化、jump/preview アクション行、Active 系譜の可視化を実装し、サイドバーの視認性を仕上げる。

**Architecture:** すべて表示層(`render.rs` / `tui.rs`)と行構築(`tree.rs`)の変更で完結する。Active 判定は `PaneSnapshot` が既に持つ `window_active` / `session_attached` から `tree.rs` で導出するため daemon の新規ポーリングは不要。クリックは Task 4 で「行単位+ダブルクリック判定」から「行+列範囲の即時判定」に置き換える。theme 新キーは 5 つ: `colors.pin` / `colors.category` / `colors.header_mode` / `colors.active_bg` / `colors.active_bar`。

**Tech Stack:** Plan 13〜22 と同じ(新規依存なし)

## DoD

### 機能完了条件

- [ ] pin 中の chat 行のマーカーが `✦`(既定ラベンダー、`colors.pin`)になり、pin 中の meta 1行にも `✦ ` が付く
- [ ] ヘッダーの mode が `≣ {mode}`(既定ラベンダー、`colors.header_mode`)、category 行が `◆ {name}`(既定ピーチ、`colors.category`)、repo 行が青のままとなり、3役が色+グリフで区別できる
- [ ] ByCategory 表示で category 行の残余幅が `─` の罫線で埋まり、グループ境界が線で読める
- [ ] LIVE が幅 24 列以上で `╭╴LIVE …╮ │…│ ╰─╯` の角丸ボーダーカードになる(枠は marker 色、`LIVE` ラベルは live 色のまま)。幅 24 未満は従来表示
- [ ] fisheye フル展開の末尾行が `↗ jump   ⌕ preview` の 1 行 2 ボタンになり、それぞれのクリックでジャンプ / popup プレビューが即時実行される
- [ ] detail 行(グレー文字)のクリックが「親 chat の手動展開トグル」になり、preview は開かない。ダブルクリック判定(250ms 待ち)が全廃され、chat 行クリック(pin)も即時反応する
- [ ] ユーザーが見ている window の agent(`window_active && session_attached`)の chat 行に薄背景(`colors.active_bg`)、その系譜(category / repo / chat / detail / jump)に左端バー `▎`(`colors.active_bar`)が付く。カーソル行では選択ハイライトが優先される

### テスト完了条件

- [ ] `rtk cargo test` 全通過(既存テストの期待値更新を含む)
- [ ] 新規テスト: pin グリフ描画、row_style の Category/Repo 分離、mode セグメントのグリフとスタイル、category 罫線、LIVE ボーダー(広幅/狭幅)、compute_areas のボーダー分確保、jump 行の列 hit-test、detail クリックの展開トグル(runtime)、active 導出と祖先伝播、active 行の bar/bg 描画、新 config キーのパース
- [ ] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [ ] `docs/e2e-smoke.md` に 6 項目の確認手順と smoke 実施結果を追記
- [ ] README に新 config キー(`colors.pin` / `colors.category` / `colors.header_mode` / `colors.active_bg` / `colors.active_bar`)の説明を追記
- [ ] `docs/sidebar-improvements-proposals.html` に対応する項目が全て実装済みであることを本計画書のチェックで担保(HTML 自体の更新は不要)

---

## Task 0: pin 強調(✦ + colors.pin)

**Files:**
- Modify: `src/config/mod.rs`(`SidebarColorsConfig` 323-342行)
- Modify: `src/sidebar/render.rs`(`SidebarRenderTheme` 9-118行、`render_row_line` 478-602行)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests:

```rust
#[test]
fn pinned_chat_row_shows_pin_glyph() {
    // 既存テストの chat 行ビルダーを流用し、meta.pinned = Some(true) の行と
    // Some(false) の行を render_rows(幅40)して文字列比較する
    // pinned:  " ✦▸ codex · repo" のように「indent の後・marker の前」に ✦
    // 非pin:   "  ▸ codex · repo"(従来どおり半角スペース)
}

#[test]
fn pin_color_is_configurable() {
    let mut config = crate::config::SidebarColorsConfig::default();
    config.pin = Some("magenta".to_string());
    let theme = SidebarRenderTheme::from_config(&config);
    assert_eq!(theme.pin, Color::Magenta);
    assert_eq!(
        SidebarRenderTheme::default().pin,
        Color::Indexed(147) // lavender
    );
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render`
Expected: コンパイルエラー(`theme.pin` / `config.pin` 未定義)

- [ ] **Step 3: 実装**

`src/config/mod.rs` の `SidebarColorsConfig` にフィールド追加:

```rust
    pub pin: Option<String>,
```

`src/sidebar/render.rs`:

1. `SidebarRenderTheme` に `pub pin: Color` を追加。`Default` は `Color::Indexed(147)`、`from_config` に `pin: parse_color(config.pin.as_deref()).unwrap_or(default.pin),` を追加。
2. `render_row_line` の Chat 行: 現在 head 文字列に pin を埋め込んでいる箇所(`format!("{indent}{pin}{marker} ")`、506-516行)を、幅計算用の head 文字列はそのまま(pin 桁は 1 桁のまま)にしつつ、span 生成を分割する:

```rust
    // spans 構築部(556-559行)を Chat だけ 3 span に分ける
    let mut spans: Vec<Span<'static>> = Vec::new();
    if row.kind == SidebarRowKind::Chat {
        let marker = if row.expanded { "▾" } else { "▸" };
        let pinned = row
            .meta
            .as_ref()
            .and_then(|meta| meta.pinned)
            .unwrap_or(false);
        spans.push(Span::styled(
            format!(" {indent}"),
            Style::default().fg(theme.marker),
        ));
        spans.push(Span::styled(
            if pinned { "✦" } else { " " }.to_string(),
            Style::default().fg(theme.pin),
        ));
        spans.push(Span::styled(
            format!("{marker} "),
            Style::default().fg(theme.marker),
        ));
    } else {
        spans.push(Span::styled(
            format!(" {head}"),
            Style::default().fg(theme.marker),
        ));
    }
```

(head 変数は label_budget の幅計算にのみ使い続ける。Chat の head は従来どおり `{indent}{pin1桁}{marker} ` 相当の幅になるよう維持する。)

3. pin 中の meta 1行(fisheye 中段、id が `meta::` で始まる Detail 行)の先頭に `✦ ` を付ける。`render_row_line` の Detail 分岐で:

```rust
        SidebarRowKind::Detail => {
            if row.id.starts_with("meta::") {
                // pin 中の要約行: pin 色の ✦ を先頭に付ける
                // head は "{indent}✦ " 相当の幅、span は indent / "✦ " で分割
            }
            indent.clone()
        }
```

具体的には head を `format!("{indent}✦ ")` にし、spans を `" {indent}"`(marker色)+ `"✦ "`(pin色)に分割する。`meta::` 以外の Detail は従来どおり。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test && rtk cargo clippy --all-targets`

```bash
rtk git add -A
rtk git commit -m "pin マーカーを ✦ + colors.pin で強調する"
```

---

## Task 1: mode / category / repo の3役色分け

**Files:**
- Modify: `src/config/mod.rs`(`SidebarColorsConfig`)
- Modify: `src/sidebar/render.rs`(`row_style` 948-960行、`mode_segment_style` 399-405行、mode セグメントのラベル生成、category ラベル)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests:

```rust
#[test]
fn category_and_repo_rows_use_distinct_colors() {
    let theme = SidebarRenderTheme::default();
    let category = row_of_kind(SidebarRowKind::Category); // 既存テストのヘルパを流用
    let repo = row_of_kind(SidebarRowKind::Repo);
    assert_eq!(row_style(&category, &theme).fg, Some(Color::Indexed(215)));
    assert_eq!(row_style(&repo, &theme).fg, Some(Color::Blue));
}

#[test]
fn mode_segment_uses_header_mode_color_and_glyph() {
    let theme = SidebarRenderTheme::default();
    assert_eq!(
        mode_segment_style(&theme).fg,
        Some(Color::Indexed(147))
    );
    // ヘッダーテキストが " ≣ repo · ≡…" になる(既存ヘッダーテストの期待値を更新)
}

#[test]
fn category_row_label_has_diamond_prefix_in_standard_tier() {
    // 幅40 で render_rows し、category 行が "▾ ◆ dev" を含むことを確認。
    // 幅30(Dense)では "◆" が付かないことも確認
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render`
Expected: FAIL / コンパイルエラー

- [ ] **Step 3: 実装**

1. `SidebarColorsConfig` に `pub category: Option<String>` / `pub header_mode: Option<String>` を追加。
2. `SidebarRenderTheme` に `pub category: Color`(既定 `Color::Indexed(215)` ピーチ)/ `pub header_mode: Color`(既定 `Color::Indexed(147)` ラベンダー)を追加し `from_config` を配線。
3. `row_style` の Category / Repo をまとめている分岐(953-955行)を分離:

```rust
        SidebarRowKind::Category => Style::default()
            .fg(theme.category)
            .add_modifier(Modifier::BOLD),
        SidebarRowKind::Repo => Style::default().fg(theme.repo).add_modifier(Modifier::BOLD),
```

4. `mode_segment_style`(399-405行)の fallback を `theme.repo` から `theme.header_mode` に変更(`sidebar.header` 明示設定時の優先は現状維持)。
5. ヘッダーの mode セグメントのラベルを `view_mode_label(mode)` から `format!("≣ {}", view_mode_label(mode))` に変更する(`build_header_layout_with_counts` 内の mode セグメント生成箇所)。hit-test はセグメント範囲ベース(`visible_segment_range`)なので自動追従するが、ヘッダーテキストを直接比較している既存テストの期待値(` repo · ≡…` → ` ≣ repo · ≡…`)を更新する。
6. category 行のラベル装飾は render 側で行う。`render_row_line` で label を truncate する前に:

```rust
    let label_source = if row.kind == SidebarRowKind::Category {
        format!("◆ {}", row.label)
    } else {
        row.label.clone()
    };
    let label = truncate_display(&label_source, label_budget);
```

Dense ティア(`render_dense_lines`)には手を入れない(グリフは Standard 限定)。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "mode/category/repo を3役3色+グリフで区別する"
```

---

## Task 2: category 行の rule 一体型区切り

**Files:**
- Modify: `src/sidebar/render.rs`(`render_row_line` の filler 生成 582-587行)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests:

```rust
#[test]
fn category_row_fills_remaining_width_with_rule() {
    // 幅40 で category 行を render_rows し、
    // "▾ ◆ dev ────…──" のように label の後ろが ─ で埋まることを確認。
    // 右端に ▲n がある場合は "…── ▲1 " と rule と右ラベルの間に空白1つ。
}

#[test]
fn repo_and_chat_rows_keep_space_filler() {
    // repo 行 / chat 行の filler は従来どおり空白のまま(─ を含まない)
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render`
Expected: FAIL

- [ ] **Step 3: 実装**

`render_row_line` の filler 生成(582-587行)を分岐:

```rust
    let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    let filler = width
        .saturating_sub(1)
        .saturating_sub(used)
        .saturating_sub(right_width);
    if row.kind == SidebarRowKind::Category && filler > 2 {
        // label と rule / rule と右ラベルの間に空白を1つずつ挟む
        spans.push(Span::styled(
            format!(" {} ", "─".repeat(filler.saturating_sub(2))),
            Style::default().fg(theme.marker),
        ));
    } else {
        spans.push(Span::raw(" ".repeat(filler)));
    }
```

Repo 行には適用しない(git バッジがあるため据え置き。必要になったら別途判断)。選択ハイライトは line 全体の bg なので rule 上にも乗る(問題なし)。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "category 行を rule 一体型の区切り見出しにする"
```

---

## Task 3: LIVE のカード化(角丸ボーダー)

**Files:**
- Modify: `src/sidebar/tui.rs`(`render_live_lines` 663-713行、`compute_areas` 825-846行、draw 側の呼び出し)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/tui.rs` tests:

```rust
#[test]
fn live_card_has_rounded_border_when_wide() {
    // 幅40, live_rows=5(本文3+枠2)で render_live_lines(width=40 を渡す)
    // lines[0] が "╭╴" で始まり "╮" で終わる(LIVE ラベル・pane id は枠上辺に埋め込み)
    // 本文行が "│ " で始まり "│" で終わる
    // 最終行が "╰" + "─"repeat + "╯"
}

#[test]
fn live_card_falls_back_to_plain_when_narrow() {
    // 幅20 では従来どおり lines[0] == " LIVE …"(枠なし)
}

#[test]
fn compute_areas_reserves_border_rows_when_wide() {
    // 幅40 高さ24, live_lines=3 → live_rows == 5(本文3+上下枠2)
    // 幅20 高さ24, live_lines=3 → live_rows == 4(本文3+見出し1、従来)
}
```

(既存の `compute_areas_reserves_live_rows_when_enabled`(1192行)の期待値を幅に応じて更新する。)

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::tui`
Expected: FAIL

- [ ] **Step 3: 実装**

1. カード化の閾値を定数化: `const LIVE_CARD_MIN_WIDTH: u16 = 24;`
2. `compute_areas`(825-846行): live のクローム行数を幅で切り替える:

```rust
    let live_chrome: u16 = if area.width >= LIVE_CARD_MIN_WIDTH { 2 } else { 1 };
    let live_rows = if live_lines > 0 && area.width > 2 && area.height >= 14 {
        (live_lines + live_chrome).min(remaining.saturating_sub(footer_rows))
    } else {
        0
    };
```

3. `render_live_lines` に `width: u16` 引数を追加し、`width >= LIVE_CARD_MIN_WIDTH` のとき:
   - `body_limit = live_rows.saturating_sub(2)`
   - 先頭行: `╭╴`(marker色)+ `LIVE`(live色+BOLD)+ ` tail · %12 `(detail色)+ 残余を `─`(marker色)+ `╮`。`title` の表示幅を `display_width` で測って `─` の本数を `width - 使用幅 - 1` で算出する
   - 本文行: 既存の ANSI 変換(`into_text`)で得た `Line` に対し、先頭に `Span::styled("│ ", marker色)` を insert、末尾に `─` ではなく空白 padding + `Span::styled("│", marker色)` を push。本文の許容幅は `width - 4`(`│␣` + `␣│`)。ANSI 行が許容幅を超える場合は span 単位で `display_width` を積算し超過 span を truncate する(全角は `truncate_display` と同じ規則)
   - 本文が `body_limit` に満たない場合も `│(空白)│` の空行で埋め、カードの高さを一定にする
   - 最終行: `╰` + `─`.repeat(width-2) + `╯`(marker色)
   - Events モードも同じ枠で描画する(ラベルが `EVENTS` になるだけ)
4. `width < LIVE_CARD_MIN_WIDTH` は従来コード(見出し1行+本文)をそのまま通す。
5. draw 側(`draw_snapshot` / live 領域の描画箇所)で `render_live_lines` に幅を渡す。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "LIVE を角丸ボーダーのカード表示にする"
```

---

## Task 4: jump/preview アクション行とクリック整理

**Files:**
- Modify: `src/sidebar/render.rs`(`render_row_line` の Jump 分岐、hit-test 関数新設)
- Modify: `src/sidebar/tui.rs`(`ClickTracker` 433-495行の撤去、`handle_left_click` 878-932行)
- Modify: `src/daemon/runtime.rs`(`ToggleRow` 399-406行)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests:

```rust
#[test]
fn jump_row_renders_two_action_buttons() {
    // Jump 行(depth=2)を幅40で描画し、" ↗ jump   ⌕ preview" 相当
    //(先頭 " " + indent 4 + "↗ jump" + 空白3 + "⌕ preview")になることを確認
}

#[test]
fn jump_row_hit_test_maps_columns_to_actions() {
    let row = jump_row(2); // depth=2, pane_id=%1
    // " " + "    "(indent) の後: columns 5..11 が jump、14..23 が preview
    assert_eq!(jump_row_action_at(&row, 5), Some(JumpRowAction::Jump));
    assert_eq!(jump_row_action_at(&row, 10), Some(JumpRowAction::Jump));
    assert_eq!(jump_row_action_at(&row, 12), None); // ボタン間の空白
    assert_eq!(jump_row_action_at(&row, 14), Some(JumpRowAction::Preview));
    assert_eq!(jump_row_action_at(&row, 22), Some(JumpRowAction::Preview));
    assert_eq!(jump_row_action_at(&row, 30), None);
}
```

`src/daemon/runtime.rs` tests:

```rust
#[test]
fn toggle_on_detail_row_toggles_manual_expand_of_parent_chat() {
    // panes に %1 を1体入れた RuntimeState で
    // apply_key("toggle:detail::%1::prompt") を実行:
    // - selection == Some("chat::%1")
    // - state.is_expanded_with_default("chat::%1", false) == true(手動展開 ON)
    // もう一度実行すると手動展開 OFF に戻る。
    // meta::%1 でも同様に働く。
    // 従来の toggle:chat::%1(pin トグル)は挙動不変。
}
```

`src/sidebar/tui.rs` tests: `ClickTracker` 系の既存テストを「即時判定」の期待に書き換える(Chat クリック → 即 ToggleRow、Detail クリック → 即 ToggleRow(detail id)、ダブルクリックの概念なし)。

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render && rtk cargo test --lib daemon::runtime`
Expected: コンパイルエラー(`jump_row_action_at` 未定義ほか)

- [ ] **Step 3: 実装**

`src/sidebar/render.rs`:

1. Jump 行の描画を専用化。`render_row_line` の冒頭(Zone 早期 return の直後)に追加:

```rust
    if row.kind == SidebarRowKind::Jump {
        let indent = "  ".repeat(row.depth);
        let mut line = Line::from(vec![
            Span::raw(format!(" {indent}")),
            Span::styled("↗ jump", Style::default().fg(Color::Cyan)),
            Span::raw("   "),
            Span::styled("⌕ preview", Style::default().fg(theme.pin)),
        ]);
        if selected {
            line = line.style(
                Style::default()
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD),
            );
        }
        return line;
    }
```

(既存の `SidebarRowKind::Jump => format!("{indent}-> ")` head 分岐と `row_style` の Jump 分岐は残っていても到達しなくなるが、`row_style` は他ティアが使うため削除しない。)

2. 列 hit-test を pure 関数で新設(テスト可能な形):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpRowAction {
    Jump,
    Preview,
}

/// Jump 行のクリック列 → アクション。レイアウトは
/// " " + indent(2*depth) + "↗ jump"(6) + "   " + "⌕ preview"(9)
pub fn jump_row_action_at(row: &SidebarRow, column: u16) -> Option<JumpRowAction> {
    if row.kind != SidebarRowKind::Jump {
        return None;
    }
    let jump_start = 1 + 2 * row.depth;
    let jump_end = jump_start + 6;
    let preview_start = jump_end + 3;
    let preview_end = preview_start + 9;
    let column = column as usize;
    if (jump_start..jump_end).contains(&column) {
        Some(JumpRowAction::Jump)
    } else if (preview_start..preview_end).contains(&column) {
        Some(JumpRowAction::Preview)
    } else {
        None
    }
}
```

`src/sidebar/tui.rs`:

3. `ClickTracker` / `PendingClick` / `ClickDecision` / `DOUBLE_CLICK_MAX` / `flush_due` と、run loop 内の `clicks.flush_due(...)` 呼び出しを削除する。`single_click_action` を即時判定に書き換え:

```rust
fn single_click_action(row: &ClickedRow) -> Option<ClickAction> {
    match row.kind {
        SidebarRowKind::Category | SidebarRowKind::Repo | SidebarRowKind::Chat => {
            Some(ClickAction::ToggleRow(row.id.clone()))
        }
        // detail / meta 行クリック = 親 chat の手動展開トグル(daemon 側で解決)
        SidebarRowKind::Detail => Some(ClickAction::ToggleRow(row.id.clone())),
        SidebarRowKind::Jump | SidebarRowKind::Zone => None,
    }
}
```

4. `handle_left_click`: clicked 行の解決後、Jump 行は列で分岐、それ以外は即時 dispatch:

```rust
    if clicked.kind == SidebarRowKind::Jump {
        match jump_row_action_at(clicked, column) {
            Some(JumpRowAction::Jump) => {
                if let Some(pane_id) = clicked.pane_id.clone() {
                    dispatch_click_action(context, ClickAction::JumpPane(pane_id));
                }
            }
            Some(JumpRowAction::Preview) => {
                if let Some(pane_id) = clicked.pane_id.clone() {
                    dispatch_click_action(context, ClickAction::PreviewPane(pane_id));
                }
            }
            None => {}
        }
        return Ok(());
    }
    if let Some(action) = single_click_action(&ClickedRow::from_row(clicked)) {
        dispatch_click_action(context, action);
    }
```

`src/daemon/runtime.rs`:

5. `ToggleRow`(399-406行)に detail/meta の解決を追加:

```rust
            SidebarInputAction::ToggleRow(row_id) => {
                if let Some(rest) = row_id
                    .strip_prefix("detail::")
                    .or_else(|| row_id.strip_prefix("meta::"))
                {
                    // detail::%1::prompt / meta::%1 → 親 chat の手動展開をトグル
                    let pane = rest.split("::").next().unwrap_or(rest);
                    let chat_id = format!("chat::{pane}");
                    self.ui_state.selection = Some(chat_id.clone());
                    self.ui_state.toggle_expanded(&chat_id)
                } else {
                    self.ui_state.selection = Some(row_id.clone());
                    if row_id.starts_with("chat::") {
                        self.ui_state.toggle_pinned(&row_id)
                    } else {
                        self.ui_state.toggle_expanded(&row_id)
                    }
                }
            }
```

**仕様メモ(コメントとして runtime に残すこと):** fisheye は `expanded = selected || manual`(tree.rs:387)のため、選択中 chat の detail をクリックしても見た目は展開のまま変わらない。このトグルは「選択を外しても展開を維持するか」の固定/解除として機能する。キーボードの `Enter`(Detail 行 → preview)と `p` は現状維持。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "jump/preview を1行2ボタン化しクリックを即時判定にする"
```

---

## Task 5: Active 系譜の可視化

**Files:**
- Modify: `src/sidebar/tree.rs`(`AgentPane` 55-73行、`SidebarRow` 23-37行、行構築)
- Modify: `src/sidebar/render.rs`(theme 2色、`render_row_line` / `render_dense_lines`)
- Modify: `src/config/mod.rs`(`SidebarColorsConfig`)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/tree.rs` tests:

```rust
#[test]
fn active_pane_marks_chat_row_and_ancestors() {
    // PaneSnapshot を2体用意: %1 は window_active=true / session_attached=true、
    // %2 は window_active=false。ByCategory で build_rows_ctx し、
    // - chat::%1 とその category / repo 行の active == true
    // - chat::%2 とその祖先(別グループ)の active == false
    // %1 を選択して展開した場合、detail::%1::* / jump::%1 も active == true
}

#[test]
fn detached_session_is_not_active() {
    // window_active=true でも session_attached=false なら active == false
}
```

`src/sidebar/render.rs` tests:

```rust
#[test]
fn active_rows_render_left_bar_and_chat_bg() {
    // active な chat 行: 先頭文字が "▎"(colors.active_bar、既定 Indexed(147))で、
    // 行スタイルの bg が theme.active_bg(既定 Indexed(235))
    // active な category 行: 先頭 "▎" のみ(bg なし)
    // 選択中の active 行: bg は selection_bg が優先
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar`
Expected: コンパイルエラー(`SidebarRow.active` 未定義ほか)

- [ ] **Step 3: 実装**

`src/sidebar/tree.rs`:

1. `AgentPane` に `active: bool` を追加し、構築箇所(177行付近)で `active: pane.window_active && pane.session_attached,` を設定(`pane` は `&PaneSnapshot`。フィールドは `src/options/snapshot.rs:20-22` に既存)。
2. `SidebarRow` に `#[serde(default)] pub active: bool` を追加(NDJSON 互換のため default 必須)。**既存の全 `SidebarRow { ... }` リテラルに `active: false` を追加**(tree.rs 内の Zone/Category/Repo/Detail/Jump/meta と、テストのフィクスチャ)。
3. active の設定:
   - chat 行(`push_chat_row` 441-453行、TRIAGE 内 chat 392-404行): `active: pane.active`
   - `detail_row` / `push_meta_row` / jump 行(553-565行): `active: pane.active`
   - category / repo 行: グループ構築時に `panes.iter().any(|pane| pane.active)` を渡す
4. TRIAGE の Zone 見出し行は `active: false` のまま。

`src/config/mod.rs`: `SidebarColorsConfig` に `pub active_bg: Option<String>` / `pub active_bar: Option<String>` を追加。

`src/sidebar/render.rs`:

5. `SidebarRenderTheme` に `pub active_bg: Color`(既定 `Color::Indexed(235)`。selection_bg=237 より暗くして選択と区別)/ `pub active_bar: Color`(既定 `Color::Indexed(147)`)を追加、`from_config` 配線。
6. `render_row_line`: 先頭の 1 桁(現在 `" "` 固定)を active バーに差し替える。Task 0 で分割した spans 構築で、各分岐の先頭 span `" {…}"` の先頭文字を `if row.active { "▎" } else { " " }` にし、`"▎"` 部分だけ `Span::styled` で `theme.active_bar` を付ける(indent 以降は marker 色のまま)。Jump 行の専用描画(Task 4)も同様に先頭を差し替える。
7. 行末のスタイル適用(593-601行)を優先順位付きに:

```rust
    let mut line = Line::from(spans);
    if selected {
        line = line.style(
            Style::default()
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD),
        );
    } else if row.active && row.kind == SidebarRowKind::Chat {
        line = line.style(Style::default().bg(theme.active_bg));
    }
    line
```

8. `render_dense_lines`: 各行の先頭 1 桁に同じバー差し替えを適用する(bg は Standard の chat 行のみ。Micro / Rail は変更なし)。

**仕様メモ(計画済みの割り切り):** 複数 client が別セッションにアタッチしている場合は active が複数系譜に付く。`window_active && session_attached` は「いずれかの client が見ている window の agent」を意味し、これを仕様とする(client 単位の絞り込みはしない)。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test && rtk cargo clippy --all-targets`

```bash
rtk git add -A
rtk git commit -m "active な系譜に左バーと薄背景を付ける"
```

---

## Task 6: 品質ゲート・smoke・ドキュメント

- [ ] **Step 1: 品質ゲート**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`
Expected: すべて通過(fmt 差分ゼロ・clippy 警告ゼロ)

- [ ] **Step 2: バイナリ反映と smoke**

```bash
cargo install --path . --force
```

scratch tmux で確認(サイドバーは `M-e` ×2 で再起動して新バイナリを反映):

- pin(`Space`)で `✦` がラベンダーで表示され、pin 解除で消える
- ヘッダー `≣ {mode}` がラベンダー、category 行 `◆ {name}` がピーチ、repo 行が青で一目で区別できる
- ByCategory で category 行の右側が `─` で埋まり、グループ境界が線で読める
- LIVE が `╭╴LIVE …╮` のカードになり、ANSI 色・入力欄カットが従来どおり機能する。`e` で EVENTS も同じ枠で表示される
- 選択 chat の末尾に `↗ jump   ⌕ preview` が出て、jump クリックでジャンプ、preview クリックで popup プレビューが**即時**開く
- detail 行クリックで手動展開がトグルされ(選択を外して確認)、preview が開かないこと。chat 行クリック(pin)に 250ms の遅延がないこと
- 自分がいる window の agent の系譜に `▎` バー、chat 行に薄背景が付く。カーソルを乗せると選択ハイライトが優先される。別 window へ移ると約1秒で追従する

結果を `docs/e2e-smoke.md` に追記。

- [ ] **Step 3: docs 更新とコミット**

- README: `sidebar.colors` の新キー 5 つ(`pin` / `category` / `header_mode` / `active_bg` / `active_bar`)を既存の colors 説明に追記
- 本計画書の DoD チェックを更新

```bash
rtk git add -A
rtk git commit -m "Plan 23 の smoke 結果と docs を更新する"
```

## スコープ外

- repo 行への rule 適用(git バッジとの同居レイアウトは必要になったら別計画)
- Micro / Rail ティアへの active 表示(幅が足りないため見送り)
- キーボード `Enter`(Detail 行)の意味変更(現状の preview を維持)
- `sidebar.live.border` の config 化(幅による自動切替のみ。要望が出たら追加)
- 旧 `ClickedRow` 構造の整理を超えるリファクタリング

## 実装ノート

(実装完了時に、計画からの差分・判断をここに追記する)
