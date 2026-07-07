# Plan 26: Sidebar ヘッダーのリッチ化(案E: powerline バッジ + フィルタチップ)

## 背景 / 目的

現状の sidebar ヘッダーは「モードバッジ・フィルタ・カウント」が1行に同じ視覚ウェイトで並び、背景塗りがなく階層が読めない。また次の2つの構造的な問題がある。

1. **カウントがフィルタ適用後の rows から算出されている**: filter は daemon 側 `tree.rs` の `build_rows_ctx` 内(`panes.retain(pane_matches_filter)`)で適用済みのため、tui の `BadgeCounts::from_rows(&sidebar.rows)` はフィルタ中に他状態の件数を正しく出せない。
2. **rows が空になるとヘッダーごと消える**: `tui.rs` の `draw` が `sidebar.rows.is_empty()` で `"no agents"` プレースホルダのみ描画して early return するため、フィルタ適用中に0件化すると「何のフィルタ中か」が分からなくなる。

本計画で以下を実装する。

- ヘッダーを **案E**(1行目: powerline モードバッジ + 総数セグメント、2行目: フィルタチップ)の2段構成にする
- **カウントをフィルタ適用前の全 panes(TRIAGE 含む)から算出**して snapshot に載せる
- **0件のフィルタは適用不可**にする(クリック・`tab` サイクルの両方。`all` は常に適用可)
- **適用中に0件化してもフィルタを自動解除せず、ヘッダーを維持**し、フィルタ文脈付きの空状態を表示する

## 参照

- ビジュアル仕様(採用モック・3状態 + 実装ノート): `docs/sidebar-header-proposals.html`
- 主要コード:
  - `src/sidebar/render.rs` — `BadgeCounts`(197)、`build_header_layout_with_counts`(282)、`header_filter_item`(415)、`header_hit_test`(445)、`render_header_lines`(455)、`view_mode_label_padded`(1321)、theme 既定値(73-95)
  - `src/sidebar/tui.rs` — 空 rows early return(515-517)、`BadgeCounts::from_rows` 呼び出し(196 / 523 / 958)、ヘッダークリック処理(958-974)
  - `src/sidebar/tree.rs` — filter 適用(223-225)、`pane_matches_filter`(690)
  - `src/daemon/runtime.rs` — `build_rows_ctx` 呼び出し(257)、`SetFilter`/`ToggleFilter` 処理(368-374)
  - `src/daemon/mod.rs` — `SidebarFrame`(59-61)
  - `src/sidebar/state.rs` — `StatusFilter::next`(273-281)
  - `src/config/schema.rs` — `sidebar.header` スキーマ(137, テスト 253-262)

## 仕様サマリ

### ヘッダーレイアウト(2行)

```
 ≣ category  7 tasks          ← 1行目: powerline セグメント
 ≡ all 7  ▲ 1  ● 1  ✓ 0  ○ 5   ← 2行目: フィルタチップ
```

- **1行目**
  - モードバッジ ` ≣ category `: bg=`header_mode`(147)、fg=暗色(反転用に theme へ fg を追加)、bold。クリックで `CycleViewMode`(既存)。`view_mode_label_padded` の固定幅を維持する。
  - powerline 矢印 ``(U+E0B0): fg=直前セグメント bg、bg=直後セグメント bg(行末は default)。
  - 総数セグメント ` {total} tasks `: bg=`active_bg`(235)、fg=`detail`(246)。非インタラクティブ。
  - 幅不足時は総数セグメントから省略し、最後は既存の `truncate_display` で切る。
- **2行目(チップ)**
  - チップ = 前後1スペースの矩形 bg 塗り。チップ間は1スペース。
  - `all` チップは常に `≡ all {n}` 表記、状態チップは `▲ {n}` 形式(グリフ+件数)。
  - アクティブチップ: bg=状態色(`all` は `header_mode`)、fg=暗色反転、bold。**0件でも反転表示を維持**(`▲ 0`)。
  - 非アクティブ・非0チップ: bg=`active_bg`(235)、fg=状態色。
  - 0件チップ: bg なし、fg=`marker`(darkgray)、**ヒットテスト対象外**。
- 既存 `sidebar.header` config のうち `format` / `prefix` / `suffix` / `separator` は役割を失うため**廃止**する(後方互換は取らない)。`colors` / `bold` はモードバッジの上書きとして維持。powerline 無効化用に `sidebar.header.powerline: boolean`(default `true`)を追加し、無効時は矢印なしの矩形塗りのみとする。

### インタラクションルール

1. カウントは**フィルタ適用前**の全 panes(TRIAGE 含む)から算出する。
2. 0件のフィルタは適用不可: クリック(`SetFilter`)は no-op、`tab`(`ToggleFilter`)は0件状態をスキップして次の非0フィルタへ。全状態が0件なら `all`。`all` は常に適用可。
3. 適用中に0件化しても自動解除しない。rows 空でもヘッダーは描画し、チップクリック可能を維持する。
4. 空状態表示: フィルタ適用中(`all` 以外)は `no {name} agents` + dim ヒント行 `tab: next filter · click ≡ all to reset`。`all` で0件なら従来どおり `no agents` のみ。

## Task 0: バッジ件数をフィルタ適用前の全体から算出する

**Files:**
- Modify: `src/sidebar/tree.rs`(counts 算出、`build_rows_ctx` の戻り値拡張)
- Modify: `src/sidebar/render.rs`(`BadgeCounts` の移動 or 参照調整)
- Modify: `src/daemon/mod.rs`(`SidebarFrame` に `counts: BadgeCounts` 追加)
- Modify: `src/daemon/runtime.rs`(counts の保持と snapshot への反映)
- Modify: `src/sidebar/tui.rs`(`BadgeCounts::from_rows(&sidebar.rows)` 196 / 523 / 958 を `sidebar.counts` 参照へ置換)

- [x] **Step 1: 失敗するテストを書く**

`tree.rs`(または runtime)のテストで「`filter: AttentionOnly` + blocked 1 / working 1 / idle 2 の panes」から rows と counts を作り、counts が `total=4, blocked=1, working=1, idle=2` になること(= rows がフィルタ済みでも counts は全体)、TRIAGE 対象 pane も counts に含まれることを検証する。

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::tree`
Expected: FAIL(counts を返す口がまだない)

- [x] **Step 3: 実装**

1. `BadgeCounts` を `render.rs` から `tree.rs` へ移動し(re-export はしない、参照側の use を修正)、`SidebarFrame` と同等の serde derive を付ける。
2. `build_rows_ctx` で `panes.retain(...)` の**前に** counts を算出し(triage_panes 含む)、`(Vec<SidebarRow>, BadgeCounts)` を返す形にする。
3. `runtime.rs:257` で counts を保持し、snapshot の `SidebarFrame` に載せる。
4. tui の `BadgeCounts::from_rows(&sidebar.rows)` 3箇所を `sidebar.counts` に置換。`from_rows` はテストヘルパーとして残してよい。

- [x] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "sidebar のバッジ件数をフィルタ適用前の全体から算出する"
```

## Task 1: 0件のステータスフィルタを適用不可にする

**Files:**
- Modify: `src/daemon/runtime.rs`(`SetFilter` ガード、`ToggleFilter` のスキップ)
- Modify: `src/sidebar/state.rs`(必要なら counts を受け取るヘルパー追加)

- [x] **Step 1: 失敗するテストを書く**

- `SetFilter(DoneOnly)` を done=0 の counts 下で適用しても filter が変わらない
- `SetFilter(All)` は常に適用される
- `ToggleFilter`(tab)が done=0 をスキップして `Working → Idle` と遷移する
- 全状態0件のとき `ToggleFilter` は `All` に落ち着く
- 適用中の filter が0件化しても自動で `All` に戻らない(状態遷移をどこにも書かないことの回帰テスト)

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib daemon`
Expected: FAIL

- [x] **Step 3: 実装**

`runtime.rs:371-374` で counts(Task 0 で保持済み)を参照する。

- `SidebarInputAction::SetFilter(filter)`: `filter != All` かつ対象 count が 0 なら no-op(bump しない)。
- `SidebarInputAction::ToggleFilter`: `StatusFilter::next()` を count が 0 の間繰り返して次の非0フィルタを決め、`set_filter` を呼ぶ(`All` は常に候補なので無限ループしない)。`state.apply(SidebarAction::ToggleFilter, ...)` 経由をやめて runtime 側で決定してよい。

- [x] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "0件のステータスフィルタを適用不可にする"
```

## Task 2: ヘッダーを powerline バッジ + フィルタチップの2段構成にする

**Files:**
- Modify: `src/sidebar/render.rs`(レイアウト本体、`HeaderSegment.action` の Option 化、theme 拡張)
- Modify: `src/config/schema.rs` / `src/config/mod.rs`(`header.format/prefix/suffix/separator` 廃止、`header.powerline` 追加)
- Modify: `src/sidebar/tui.rs`(ヒットテスト呼び出し側の追従。`header.row_count()` ベースの既存処理は原則そのまま動く想定)

- [ ] **Step 1: 失敗するテストを書く**

`render.rs` tests(既存の 1997 / 2021 / 2102 / 2129 / 2172 / 2216 系は仕様変更に合わせて書き換え or 削除):

- レイアウトが2行になり、1行目に `≣ category` と `7 tasks`、2行目にチップ列が並ぶ(テキスト全体の assert)
- ヒットテスト: 1行目モードバッジ → `CycleViewMode`、総数セグメント → `None`、2行目各チップ → `SetFilter(...)`、0件チップ座標 → `None`
- アクティブチップの Style が「状態色 bg + 反転 fg + BOLD」、非アクティブ非0チップが「`active_bg` bg + 状態色 fg」、0件チップが「bg なし + marker fg」
- `powerline: false` 時に矢印グリフが含まれない
- 幅を絞ると総数セグメントが落ち、さらに絞ると truncate される

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render`
Expected: FAIL

- [ ] **Step 3: 実装**

1. `HeaderSegment.action` を `Option<HeaderAction>` にし、`header_hit_test` は `None` の segment を無視する。矢印・総数セグメントも style 付き segment として表現する(action は `None`)。
2. `build_header_layout_with_counts` を仕様サマリどおり2行構成に書き換える。`view_mode_label_padded` は維持。
3. theme に反転用 fg(例: `header_badge_fg`、default は端末黒相当の `Color::Indexed(16)`)と `header_powerline: bool` を追加。`SidebarRenderTheme` の色規約コメントも更新。
4. config: `sidebar.header` から `format/prefix/suffix/separator` を削除し、`powerline`(boolean, default true)を追加。schema テスト(253-262)を更新。`colors`/`bold` はモードバッジの上書きとして残す。
5. `header_filter_item` / `format_header_segment` 等の不要になった分岐を整理する。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`(tui 側のヘッダー行数に依存するテストの期待値更新を含む)

```bash
rtk git add -A
rtk git commit -m "sidebar ヘッダーを powerline バッジとフィルタチップの2段構成にする"
```

## Task 3: フィルタ適用中の空状態でもヘッダーを維持する

**Files:**
- Modify: `src/sidebar/tui.rs`(515-517 の early return 廃止、空状態プレースホルダの文脈表示)
- Modify: `src/sidebar/render.rs`(必要なら `filter_name(StatusFilter)` ヘルパー追加)

- [ ] **Step 1: 失敗するテストを書く**

既存 `renders_no_agents_placeholder_for_empty_sidebar_rows`(tui.rs:1481)を分割・拡張:

- rows 空 + `filter: AttentionOnly` → ヘッダー(モードバッジ・チップ)が描画され、rows 領域に `no attn agents` と `tab: next filter · click ≡ all to reset` が出る
- rows 空 + `filter: All` → ヘッダーは描画され、`no agents` のみ(ヒント行なし)
- rows 空でもヘッダークリック(チップ座標)で `SetFilter` が発火する(クリック処理パスに rows 空の early return がないこと)

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::tui`
Expected: FAIL(現状はヘッダーなしで "no agents" のみ)

- [ ] **Step 3: 実装**

1. `tui.rs:515-517` の early return を廃止し、rows 空でもヘッダーを組み立てて描画、rows 領域にのみプレースホルダを出す。
2. プレースホルダ: フィルタ適用中は `no {name} agents`(name は all/attn/working/done/idle)+ dim のヒント行。`all` は従来文言。
3. `snapshot.sidebar` 自体がない場合の `no sidebar data` は従来どおり。
4. クリック処理(958-974 周辺)に rows 空で bail する分岐が残っていないか確認して除去。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "フィルタ適用中の空状態でもヘッダーを維持する"
```

## Task 4: 品質ゲート・smoke・ドキュメント

**Files:**
- Modify: `README.md`(ヘッダー仕様、`sidebar.header` config の変更点)
- Modify: `docs/e2e-smoke.md`(確認手順の追記)

- [ ] **Step 1: ドキュメント更新**

1. README: 2段ヘッダーの表示仕様、チップの状態表現(アクティブ反転 / 0件 dim・非活性)、`sidebar.header` の `format/prefix/suffix/separator` 廃止と `powerline` 追加を明記。
2. `docs/e2e-smoke.md`: 「フィルタチップのクリック切替 → 0件チップが反応しない → 適用中に0件化してもヘッダーが残り `≡ all` クリックで復帰」の smoke 手順を追記。

- [ ] **Step 2: 品質ゲート**

Run:
- `rtk cargo test`
- `rtk cargo clippy --all-targets`(警告ゼロ)
- `rtk cargo fmt --check`

- [ ] **Step 3: smoke 実施**

実際に tmux 上で sidebar を起動し、`docs/e2e-smoke.md` に追記した手順を通し、`docs/sidebar-header-proposals.html` の状態1〜3と表示が一致することを目視確認する。

- [ ] **Step 4: コミット**

```bash
rtk git add -A
rtk git commit -m "docs: sidebar ヘッダー刷新の smoke 手順と README を更新する"
```

## DoD

### 機能完了条件

- [ ] ヘッダーが2行(1行目: powerline モードバッジ + 総数セグメント、2行目: フィルタチップ)で描画される
- [ ] バッジ件数がフィルタ適用前の全 panes(TRIAGE 含む)から算出され、フィルタ適用中も他状態の件数が変わらない
- [ ] アクティブチップは状態色反転 + bold(0件化しても `▲ 0` で反転維持)、非アクティブ非0チップは `active_bg` + 状態色、0件チップは dim・クリック不可
- [ ] 0件フィルタはクリックでも `tab` サイクルでも適用されない(`all` は常に適用可、全状態0件時のサイクルは `all` に収束)
- [ ] 適用中に0件化してもフィルタは自動解除されず、ヘッダー表示とチップ操作が維持され、空状態に `no {name} agents` + 復帰ヒントが表示される
- [ ] `all` フィルタで0件のときは従来どおり `no agents` のみ表示される
- [ ] 幅不足時は総数セグメント省略 → truncate の順で退避し、モード切替でチップ位置がずれない
- [ ] `sidebar.header.powerline: false` で矢印グリフなしの矩形塗りにフォールバックする

### テスト完了条件

- [ ] 新規テスト: counts の全体算出(フィルタ・TRIAGE 込み)、`SetFilter` の0件ガード、`ToggleFilter` の0件スキップと `all` 収束、2行レイアウトのテキストとヒットテスト(0件チップ・総数セグメントの非活性含む)、チップの Style 3態、powerline off、幅フォールバック、空状態でのヘッダー維持とヒント表示、rows 空でのチップクリック
- [ ] `rtk cargo test` 全通過(既存ヘッダーテスト・config schema テストの期待値更新を含む)
- [ ] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [ ] README にヘッダー表示仕様と `sidebar.header` config の変更(`format/prefix/suffix/separator` 廃止・`powerline` 追加)を明記
- [ ] `docs/e2e-smoke.md` に確認手順を追記し、実機 smoke を実施
- [ ] `docs/sidebar-header-proposals.html` の状態1〜3と実装の表示が一致することを確認

## スコープ外

- 分布メーター(検討時の案D)の追加
- チップの角丸表現(nerd font 半円 `` / `` の利用)
- statusline 側の変更
- theme 色そのもののカスタマイズ拡張(powerline glyph の差し替え config など)

## 実装ノート

- filter は daemon 側で適用される設計(`tree.rs`)を変えない。TRIAGE ゾーンがフィルタ対象外である既存挙動も変えない。
- キーボードからフィルタを直接指定する経路はない(`input.rs` の `"all"` 等のキー名はヘッダークリック時に tui が送るもの)。キーボード操作は `tab` サイクルのみなので、0件ガードは `SetFilter` と `ToggleFilter` の両方に必要。
- `HeaderSegment.action` の Option 化はヒットテストの仕様変更(0件チップ・総数セグメント・矢印を非活性にする)のための前提。
- powerline 矢印 ``(U+E0B0)は nerd font 前提。`sidebar.header.powerline: false` で無効化できるようにする。
- アクティブチップの反転 fg は端末テーマに依存しないよう theme フィールド(default `Color::Indexed(16)` 相当の暗色)として持つ。
