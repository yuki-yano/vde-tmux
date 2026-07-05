# Plan 16: fisheye 完成と幅適応(UI 再設計 Phase 4)

> **実装者向け:** `docs/sidebar-ui-proposals.md` §9.2 Phase 4 の実装。**Plan 13〜15 完了が前提**。行高が動的になる Phase であり、scroll 制御とクリック対応の整合が品質の要。Task 順に実施する。

**Goal:** 行高3段階(選択=フル展開 / pin=中展開 / 他=1行)の fisheye を完成させ、幅ティア(dense / micro)と rail の「集計+個別」2部構成による幅適応を導入する。

**Architecture:** 「行高」は実際には daemon 側で挿入される追加 SidebarRow 群として表現する(可変高ウィジェットは使わない)。これによりクリックの行対応は常に 1:1 のまま。client 側にはスクロールオフセットを導入し、選択行(と展開行)が viewport 内に収まるよう draw 時に補正する。幅ティアは client 側 render の分岐(daemon はティアを知らない)。

**Tech Stack:** Plan 13〜15 と同じ(新規依存なし)

## DoD

### 機能完了条件

- [x] 選択中の Chat 行はフル展開(status/elapsed/session/subagents/jump の Detail 行群)が自動表示され、選択が離れると畳まれる
- [x] Space で Chat 行を pin できる(トグル)。pin 行は選択がどこにあっても中展開(meta 1行)を維持し、行頭に pin 印(`·`)が付く
- [x] Repo/Category 行の Space(グループ折りたたみ)は従来どおり
- [x] 行数が viewport を超えたとき、j/k・n/N での選択移動に追従してスクロールし、選択行が常に表示される
- [x] スクロール中のクリックが正しい行に届く(オフセット補正)
- [x] 幅 24〜35 列で dense 表示(1行/agent、`{glyph} {agent} {repo略} {right}`)に自動縮退する
- [x] 幅 3〜23 列で micro 表示(`{glyph} {right}` 相当の最小形)に縮退する
- [x] rail(幅≤2)が「状態別カウント + 罫線 + 個別グリフ」の2部構成になる
- [x] state.json に pinned が永続化され、旧 state.json の読み込みが壊れない

### テスト完了条件

- [x] `rtk cargo test` 全通過
- [x] 新規テスト: pin トグルと永続化、選択フル展開/pin 中展開/その他1行の共存、scroll 追従、オフセット込みクリック対応、各幅ティアの出力、rail 2部構成
- [x] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [x] `docs/e2e-smoke.md` に fisheye(選択で開く・pin で留まる)、幅を変えての縮退確認、スクロール確認を追記し、smoke 実施を記録
- [x] `docs/sidebar-ui-proposals.md` §9.2 Phase 4 にチェック

---

## Task 0: pin 状態と Space の意味変更

**Files:**
- Modify: `src/sidebar/state.rs`(SidebarState.pinned、toggle_pinned)
- Modify: `src/daemon/runtime.rs`(`apply_key` ToggleExpand 分岐)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/state.rs` tests:

```rust
#[test]
fn pinned_set_toggles_and_persists() {
    let mut state = SidebarState::default();
    assert!(state.toggle_pinned("chat::%1"));
    assert!(state.pinned.contains("chat::%1"));
    assert!(state.toggle_pinned("chat::%1"));
    assert!(!state.pinned.contains("chat::%1"));

    let json = serde_json::to_string(&SidebarState {
        pinned: std::iter::once("chat::%1".to_string()).collect(),
        ..SidebarState::default()
    })
    .unwrap();
    assert!(json.contains(r#""pinned""#));
    // 旧 state.json(pinned なし)も読める
    let old: SidebarState = serde_json::from_str(r#"{"version":3}"#).unwrap();
    assert!(old.pinned.is_empty());
}
```

`src/daemon/runtime.rs` tests:

```rust
#[test]
fn space_on_chat_row_toggles_pin_instead_of_expand() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    state.ui_state.selection = Some("chat::%1".to_string());

    state.apply_event(DaemonEvent::Client {
        client_id: ClientId(1),
        event: SidebarClientEvent::Key {
            key: "space".to_string(),
        },
    });

    assert!(state.ui_state.pinned.contains("chat::%1"));
    // 展開集合は変化しない(フル展開は選択駆動)
    assert!(!state.ui_state.is_expanded_with_default("chat::%1", false));
}

#[test]
fn space_on_repo_row_still_toggles_collapse() {
    // selection=repo 行で space → collapsed に repo id が入る(従来挙動)
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::state::tests::pinned_set_toggles_and_persists`
Expected: コンパイルエラー(pinned 未定義)

- [ ] **Step 3: 実装**

`src/sidebar/state.rs`:

```rust
    #[serde(default)]
    pub pinned: BTreeSet<String>,
```

を `SidebarState` に追加し、メソッド:

```rust
    pub fn toggle_pinned(&mut self, id: &str) -> bool {
        if !self.pinned.insert(id.to_string()) {
            self.pinned.remove(id);
        }
        self.bump();
        true
    }
```

`src/daemon/runtime.rs` の `apply_key` の `ToggleExpand` 分岐を置き換え:

```rust
            SidebarInputAction::ToggleExpand => {
                let is_chat = self
                    .ui_state
                    .selection
                    .as_deref()
                    .map(|id| id.starts_with("chat::"))
                    .unwrap_or(false);
                if is_chat {
                    let id = self.ui_state.selection.clone().unwrap();
                    self.ui_state.toggle_pinned(&id)
                } else {
                    self.ui_state.apply(SidebarAction::ToggleExpand, &row_refs)
                }
            }
```

`ToggleRow`(クリック)も同様に chat:: なら pin に変える。`l`/`h`(Expand/Collapse)は従来どおり残す(明示フル展開の escape hatch)。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`(Plan 14 Task 0 の「Space でフル詳細」に依存するテストがあれば pin 挙動に更新する)

```bash
rtk git add -A
rtk git commit -m "Space を chat 行の pin トグルに変更する"
```

---

## Task 1: 行高3段階の行構築(選択=フル / pin=中 / 他=1行)

**Files:**
- Modify: `src/sidebar/tree.rs`(`push_chat_row`、triage_zone_rows)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/tree.rs` tests:

```rust
#[test]
fn selected_chat_expands_full_pinned_expands_medium_others_single() {
    // 3体: %1(選択)、%2(pinned)、%3(どちらでもない)。全て prompt あり
    // state.selection=Some("chat::%1"), state.pinned={"chat::%2"}
    let rows = /* build_rows_ctx(Flat) */;
    // %1: chat 行の直後に Detail 群(status 行を含む)+ jump 行
    assert!(rows.iter().any(|row| row.id == "detail::%1::status"));
    assert!(rows.iter().any(|row| row.id == "jump::%1"));
    // %2: meta 1行のみ(detail 群は無い)
    assert!(rows.iter().any(|row| row.id == "meta::%2"));
    assert!(!rows.iter().any(|row| row.id == "detail::%2::status"));
    // %3: 追加行なし
    assert!(!rows.iter().any(|row| row.id.ends_with("%3") && row.id.starts_with("meta::")));
    assert!(!rows.iter().any(|row| row.id == "detail::%3::status"));
}

#[test]
fn pinned_rows_render_with_pin_marker() {
    // pinned な chat 行の label 先頭に "· " は付けない(render 側で付ける)が、
    // SidebarRow に pinned を伝える手段として meta.pinned == Some(true) を検証
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::tree::tests::selected_chat_expands_full_pinned_expands_medium_others_single`
Expected: FAIL

- [ ] **Step 3: 実装**

`RowMeta` にフィールド追加:

```rust
    pub pinned: Option<bool>,
```

`push_chat_row` の展開判定を再構成(Plan 14 Task 0 の分岐を置き換える):

```rust
    let selected = state.selection.as_deref() == Some(id.as_str());
    let pinned = state.pinned.contains(&id);
    let manual = state.is_expanded_with_default(&id, false); // l/h の明示展開
    let mut meta = chat_meta(pane, now);
    meta.pinned = Some(pinned);
    // (chat 行 push。meta: Some(meta))
    if selected || manual {
        push_chat_detail_rows(pane, depth + 1, now, rows);
    } else if pinned {
        rows.push(/* Plan 14 Task 0 と同じ meta 行(id: meta::{pane_id}) */);
    }
```

(選択時の meta 1行は不要になる — フル展開が meta 情報を含むため。Plan 14 の `selected → meta 行` テストは本 Task で「selected → detail 群」に期待値変更する。)

`triage_zone_rows`(Plan 15)も同じ3段階分岐を適用する。

- [ ] **Step 4: render に pin 印**

`src/sidebar/render.rs` の `render_row_line` で、Chat 行の head 生成を拡張:

```rust
        SidebarRowKind::Chat => {
            let marker = if row.expanded { "▾" } else { "▸" };
            let pin = if row
                .meta
                .as_ref()
                .and_then(|meta| meta.pinned)
                .unwrap_or(false)
            {
                "·"
            } else {
                " "
            };
            format!("{indent}{pin}{marker} ")
        }
```

render テスト追加: pinned meta 付き chat 行の出力が `" ·▸ "` 始まりになること。

- [ ] **Step 5: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "選択フル展開と pin 中展開の3段階行高にする"
```

---

## Task 2: スクロールオフセットとクリック補正

**Files:**
- Modify: `src/sidebar/tui.rs`(スクロール状態、draw、クリック)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/tui.rs` tests:

```rust
#[test]
fn scroll_follows_selection() {
    // resolve_scroll(prev_scroll, selection_index, rows_len, viewport) を直接検証
    assert_eq!(resolve_scroll(0, Some(5), 30, 10), 0); // 表示内 → 維持
    assert_eq!(resolve_scroll(0, Some(15), 30, 10), 6); // 下へはみ出し → 末尾に合わせる
    assert_eq!(resolve_scroll(6, Some(2), 30, 10), 2); // 上へはみ出し → 先頭に合わせる
    assert_eq!(resolve_scroll(25, Some(29), 30, 10), 20); // 末尾張り付き
    assert_eq!(resolve_scroll(9, None, 5, 10), 0); // rows が viewport 以下 → 0
}

#[test]
fn click_maps_through_scroll_offset() {
    // row_for_click に scroll を加味した версия:
    // rows 30件, header 1, scroll 6 のとき y=2 → rows[7]
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::tui::tests::scroll_follows_selection`
Expected: コンパイルエラー

- [ ] **Step 3: 実装**

`src/sidebar/tui.rs` に追加:

```rust
pub(crate) fn resolve_scroll(
    prev: usize,
    selection_index: Option<usize>,
    rows_len: usize,
    viewport: usize,
) -> usize {
    if viewport == 0 || rows_len <= viewport {
        return 0;
    }
    let max_scroll = rows_len - viewport;
    let mut scroll = prev.min(max_scroll);
    if let Some(index) = selection_index {
        if index < scroll {
            scroll = index;
        } else if index >= scroll + viewport {
            scroll = index + 1 - viewport;
        }
    }
    scroll.min(max_scroll)
}
```

- run loop に `let mut scroll: usize = 0;` を持たせ、snapshot 受信ごとに `selection_index = rows.iter().position(id == selection)` を計算して `scroll = resolve_scroll(scroll, selection_index, rows.len(), areas.rows_height as usize)`、draw に渡す。
- `draw_snapshot_in_area` は `scroll` を引数に取り、`render_lines(...)` の結果を `.skip(scroll).take(rows_height)` してから List にする。
- `row_for_click` を `rows.get(usize::from(row - header_rows) + scroll)` に変更(scroll を引数追加)。`handle_left_click` は run loop の scroll を参照する(ClickContext に `scroll: usize` を追加するか、引数で渡す)。
- 選択追従で十分なため、マウスホイールは本 Plan では扱わない(スコープ外に明記)。

注意: フル展開行群(選択の Detail 群)が viewport より大きい場合は選択行(Chat 行)の可視を優先する。`selection_index` は Chat 行のインデックスなので上記実装で自然に満たされる。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "sidebar にスクロール追従とクリック補正を追加する"
```

---

## Task 3: 幅ティア(dense / micro)と rail 2部構成

**Files:**
- Modify: `src/sidebar/render.rs`(WidthTier、render_lines 分岐、render_rail_lines)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests:

```rust
#[test]
fn width_tier_boundaries() {
    assert_eq!(WidthTier::from_width(2), WidthTier::Rail);
    assert_eq!(WidthTier::from_width(3), WidthTier::Micro);
    assert_eq!(WidthTier::from_width(23), WidthTier::Micro);
    assert_eq!(WidthTier::from_width(24), WidthTier::Dense);
    assert_eq!(WidthTier::from_width(35), WidthTier::Dense);
    assert_eq!(WidthTier::from_width(36), WidthTier::Standard);
}

#[test]
fn dense_tier_renders_one_line_per_chat_with_origin_abbrev() {
    // meta.origin = Some("misc/vde-tmux") の running chat(elapsed 780s)を幅 30 で render
    // 出力: " ● claude  vde fix …       13m" 形式
    // - group/zone 行は「見出しのみ」(git・右カラムなし)
    // - chat 行は {glyph} {agent(7左詰)} {repo3(dim)} {label…} {right}
    let rendered = /* render_rows(width=30) */;
    assert!(rendered.contains("● claude  vde"), "{rendered:?}");
    assert!(rendered.ends_with("13m "), "{rendered:?}");
}

#[test]
fn micro_tier_renders_glyph_and_status_only() {
    // 幅 8: " ▲ perm" 形式(agent 名なし)。Detail/meta 行は出力しない
}

#[test]
fn rail_renders_counts_then_rows() {
    // blocked 2 + working 1 の rows を幅2で render:
    // "▲2\n●1\n──\n▲\n▲\n●" (idle/done は 0 件なのでカウント行なし)
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render::tests::width_tier_boundaries`
Expected: コンパイルエラー

- [ ] **Step 3: 実装**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthTier {
    Rail,
    Micro,
    Dense,
    Standard,
}

impl WidthTier {
    pub fn from_width(width: usize) -> Self {
        match width {
            0..=2 => Self::Rail,
            3..=23 => Self::Micro,
            24..=35 => Self::Dense,
            _ => Self::Standard,
        }
    }
}
```

`render_lines` を tier 分岐に変更:

```rust
    match WidthTier::from_width(width) {
        WidthTier::Rail => render_rail_lines(rows, state, theme),
        WidthTier::Micro => render_micro_lines(rows, state, theme, width),
        WidthTier::Dense => render_dense_lines(rows, state, theme, width),
        WidthTier::Standard => rows
            .iter()
            .map(|row| render_row_line(row, state, width, theme))
            .collect(),
    }
}
```

dense / micro の実装方針(コード量が多いため要点のみ厳密化。関数構造は render_row_line に倣う):

- `render_dense_lines`: Detail/Jump/meta 行はスキップ(1行/agent を守る)。Zone/Repo/Category は ` ▍TRIAGE 2` / ` ▾ {label}` のみ(git・右カラムなし)。Chat は `" {glyph} {agent:7} {origin3} {label…} {right}"`。`origin3` は `meta.origin` の `/` 以降(repo 名)の先頭3文字を DIM で。选択 bg はフル幅(Standard と同じ filler 方式)。
- `render_micro_lines`: Chat 行のみ。`" {glyph} {right}"`(right が無ければ glyph のみ)。選択 bg 同様。
- `render_rail_lines` を置き換え: 先頭に BadgeState ごとのカウント行(`▲2` 等、0件はスキップ、色付き)、`──`(DarkGray)、続けて従来の per-chat グリフ行。

dense/micro では Detail 系がスキップされるため、**クリック・スクロールの行対応が rows インデックスとずれる**。`render_lines` が `Vec<Line>` と同時に「表示行 → rows インデックス」の対応表を返すよう戻り値を `(Vec<Line<'static>>, Vec<usize>)` に変更し、tui 側の click / scroll はこの対応表を介して解決する(Standard/Rail では恒等)。この変更は `render_rows`(テスト用)には影響させない(内部で .0 を使う)。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "幅ティアによる dense/micro 縮退と rail 2部構成を追加する"
```

---

## Task 4: smoke・ドキュメント・品質ゲート

- [ ] **Step 1: 品質ゲート**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`

- [ ] **Step 2: smoke**

scratch tmux で確認(daemon 再起動込み):
- j/k で選択を動かすと選択行だけがフル展開され、他が畳まれる
- Space で pin → 選択を離しても meta 1行が残り、行頭に `·`
- agent を10体以上並べ、スクロール追従とクリックの行一致を確認
- pane 幅を 30 → 20 → 2 と縮めて dense → micro → rail(カウント+個別)の縮退を確認
- state.json に pinned が保存され、daemon 再起動後も pin が残る

結果を `docs/e2e-smoke.md` に追記。

- [ ] **Step 3: docs 更新とコミット**

```bash
rtk git add docs/
rtk git commit -m "Plan 16 の smoke 結果と docs を更新する"
```

## スコープ外

- マウスホイールでのスクロール(選択追従で代替。要望があれば追加)
- rich ティア(幅≥56 の全行カード)→ Plan 17 完了後の Phase 6 ゲートで判断
- pin 行の高さ不足時の自動縮退(pin 数が実用上少ない想定。問題が出たら Phase 6 で)

## 実装ノート

- `capture-pane -a` は Plan 15 と同様に TUI alt-screen が空になるため、fisheye / pin / state 永続化は scratch daemon の subscribe snapshot で検証した。
- dense / micro / rail は client render の幅ティアなので、scratch tmux 上の pane option と state.json を使い、`sidebar attach --once` の幅30/20/2出力で検証した。
- scroll の実画面追従は capture できないため、`resolve_scroll` と scroll offset 付き click mapping の unit test を smoke 記録上の証跡とした。
- 選択展開は pane 系列判定に修正した(Plan 18 Task 1)。`jump::` / `detail::` に選択が移っても親 chat のフル展開を維持する。
