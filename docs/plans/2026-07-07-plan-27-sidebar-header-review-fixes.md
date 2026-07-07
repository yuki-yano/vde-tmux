# Plan 27: Sidebar ヘッダー品質レビュー指摘の修正

## 背景 / 目的

Plan 26(sidebar ヘッダーのリッチ化)の実装に対して 8 観点の品質レビューを行い、10 グループの指摘が検証で確定した。本計画はその修正を行う。

前提: working tree には**動作確認済みの未コミット変更**が存在する(レビュー中に適用した修正 + pill 機能)。内容:

- `src/sidebar/render.rs`: アクティブチップの反転 fg を `Indexed(16)` 固定から `mode_fg()`(`header.colors.fg` 優先)に変更(base16 系テーマでパレット 16 が再定義される問題の修正)、bold を `mode_segment_style` と同じ規則に変更、filter chip の pill キャップ(`header_chip_prefix/suffix`)対応、回帰テスト3件
- `src/config/mod.rs` / `src/config/schema.rs`: `sidebar.header.chip_prefix` / `chip_suffix` の追加

## 修正対象の指摘(深刻度順)

1. **[高] attn フィルタのゲートとビュー条件の不整合** — ゲート/チップは `counts.blocked` を見るが、ビューの実条件 `pane_matches_filter(AttentionOnly)`(`tree.rs:751`)は `attention || Blocked || Working`。blocked 0 件 + working N 件で attn ビューが到達不能になる
2. **[高] 旧 daemon 生存中の更新でクライアント恒久停止** — `SidebarFrame.counts`(`daemon/mod.rs:61`)に `#[serde(default)]` がなく、`client.rs` の購読スレッドは deserialize 失敗で reconnect なしに break する
3. **[中] `docs/migration.md:81,86` が廃止済み `separator` キーを例示** — コピーすると config 全体が警告のみでデフォルト化する
4. **[中] `chip_color` が `header.colors.bg` を無視** — None arm が `mode_bg()` を通らず `header_mode` 直参照(`render.rs:532` 付近)
5. **[中] チップグリフが `badge.glyphs` 設定をバイパス** — `HeaderChipSpec` の ▲●✓○ がハードコード(`render.rs:397-421`)
6. **[中] `header_total_suffix` が既定矢印と文字列一致した場合のみ後続遷移を描画** — カスタム glyph で非対称ヘッダー(`render.rs:372`)
7. **[低] `BadgeCounts::from_rows` が dead code**(呼び出し 0 件、旧バグ実装の罠)
8. **[低] 重複マッピング/述語** — `filter_name`(`tui.rs:611`)と `filter_key`(`tui.rs:1056`)、チップ action 判定(`render.rs:432`)と `filter_is_available`(`runtime.rs:803`)
9. **[低] render.rs クリーンアップ** — 到達不能な `header_segment_style` fallback、bold 式の重複、`build_header_title_line` の segment-push 4連コピペ
10. **[低] 軽微** — `1 tasks` 表記、`full_text.clone()` 等の無駄アロケーション

## Task 0: 未コミット変更をコミットする(実施済み)

**このタスクはレビューセッション内で実施済み。Codex は Task 1 から開始すること。**

- [x] **Step 1**: `rtk cargo test && rtk cargo clippy --all-targets && rtk cargo fmt --check` が通ることを確認
- [x] **Step 2**: チップの色/bold 修正をコミット(`ea23bee` アクティブチップの反転色と bold を header 設定に従わせる)
- [x] **Step 3**: pill キャップ機能をコミット(`39e48e9` filter chip に pill キャップ設定を追加する)

## Task 1: attn カウントをビュー条件と一致させる(指摘1)

**Files:** `src/sidebar/tree.rs`(BadgeCounts / badge_counts_from_agent_panes)、`src/daemon/runtime.rs`(変更不要の想定、テスト追加)、`src/sidebar/render.rs`(チップは count 経由なので自動追従)

仕様: `BadgeCounts` に `attention: usize` を追加し、`pane_matches_filter(AttentionOnly)` と**同一の述語**(`attention || Blocked || Working`)で算出する。`count_for_filter(AttentionOnly)` は `self.attention` を返す。▲ チップの count も同フィールドに切り替える(表示の意味が「blocked 数」から「attn ビュー該当数」に変わることを README に明記)。

- [x] **Step 1: 失敗するテストを書く** — blocked 0 / working 2 の panes で `counts.attention == 2`、`filter_is_available(counts, AttentionOnly) == true`、▲ チップが count 2 でクリック可能、`tab` サイクルが AttentionOnly をスキップしない
- [x] **Step 2: FAIL 確認** — `rtk cargo test --lib sidebar::tree`
- [x] **Step 3: 実装** — 述語は `pane_matches_filter` と共通の関数に切り出して二重実装を避ける(指摘8の runtime/render 述語統合はここでやってもよい)
- [x] **Step 4: テスト通過 + コミット**

```bash
rtk git add -A
rtk git commit -m "attn フィルタの件数をビュー条件と一致させる"
```

## Task 2: snapshot 互換とクライアント購読の頑健化(指摘2)

**Files:** `src/daemon/mod.rs`、`src/sidebar/tree.rs`(BadgeCounts derive)、`src/sidebar/client.rs`

仕様:

1. `SidebarFrame.counts` に `#[serde(default)]` を付ける(`BadgeCounts` は `Default` 実装済みのはず。フィールド追加にも耐える)
2. `client.rs` の購読ループ: 1行の deserialize 失敗で break せず、**warn を出してその行をスキップ**して継続する(接続エラー系の break は従来どおり)

- [x] **Step 1: 失敗するテストを書く** — `counts` を欠いた snapshot JSON が `ServerMessage` として deserialize でき、counts が default(全0)になる。client の行処理が不正 JSON 行をスキップして次の行を処理する
- [x] **Step 2: FAIL 確認**
- [x] **Step 3: 実装**
- [x] **Step 4: テスト通過 + コミット**

```bash
rtk git add -A
rtk git commit -m "snapshot の counts 欠落と不正行に耐性を持たせる"
```

## Task 3: migration.md の廃止キー除去(指摘3)

**Files:** `docs/migration.md`

- [x] `sidebar.header` の例から `separator` を削除し、現行スキーマ(`format/prefix/suffix/bold/colors/chip_prefix/chip_suffix`)に合わせて書き直す。pill の例は `chip_prefix: ""` / `chip_suffix: ""` を使う
- [x] コミット

```bash
rtk git add docs/migration.md
rtk git commit -m "docs: migration.md の sidebar.header 例を現行スキーマに合わせる"
```

## Task 4: ヘッダー色/グリフ/遷移の一貫性(指摘4・5・6)

**Files:** `src/sidebar/render.rs`、`README.md`

仕様:

1. `chip_color` の None arm を `mode_bg(theme)` 経由にする(`header.colors.bg` > `colors.header_mode` の解決順に統一)
2. `HeaderChipSpec` の状態グリフを `theme.badge_glyph(state)` で解決する(`≡` は状態ではないので据え置き)。`glyph` フィールドは削除できるはず
3. `header_total_suffix` の「既定矢印と一致した場合のみ」を廃止し、**`header_suffix` が非空なら総数セグメント後にも同じ suffix を描画**する(単純化。fg/bg の遷移色はそのまま)

- [ ] **Step 1: 失敗するテストを書く** — (a) `header_active_bg` 設定時にアクティブ all チップ bg がそれに従う (b) `badge.glyphs.working` を "W" にするとチップも "W" になる (c) `suffix` を "\u{e0b4}" にしても総数セグメント後に suffix が出る
- [ ] **Step 2: FAIL 確認** — `rtk cargo test --lib sidebar::render`
- [ ] **Step 3: 実装 + README の該当記述更新**
- [ ] **Step 4: テスト通過 + コミット**

```bash
rtk git add -A
rtk git commit -m "ヘッダーの色解決・グリフ・powerline 遷移を設定と一貫させる"
```

## Task 5: dead code と重複の整理(指摘7・8・9・10)

**Files:** `src/sidebar/render.rs`、`src/sidebar/tree.rs`、`src/sidebar/tui.rs`

- [ ] `BadgeCounts::from_rows` を削除(呼び出し 0 件)
- [ ] `filter_name`(tui.rs:611)と `filter_key`(tui.rs:1056)を単一のマッピングに統合(`StatusFilter` のメソッドにするのが素直)
- [ ] フィルタ選択可否の述語を共有化: `filter_is_available` 相当を `BadgeCounts`(または `StatusFilter`)側に移し、`runtime.rs` と `render.rs` のチップ action 判定(`All || active || count > 0`)が同じ関数を参照する形にする(active の特別扱いは render 側で OR する)
- [ ] `header_segment_style` と `render_header_lines` の `unwrap_or_else` fallback を削除(全 segment が `Some(style)` を持つ)
- [ ] bold 判定 `!header_style_configured(theme) || theme.header_active_bold` を `header_bold(theme)` ヘルパーに抽出し、`mode_segment_style` / `chip_style` の両方から使う
- [ ] `build_header_title_line` をチップ行と同じ pieces 方式(`Vec<(String, Option<Style>, Option<HeaderAction>)>` を1ループで segment 化)に統一
- [ ] `full_text.clone()`(render.rs:303 付近)を move に、`" {} tasks "` を 1 件時 `"1 task"` に修正(テスト更新)
- [ ] `rtk cargo test` / `rtk cargo clippy --all-targets` / `rtk cargo fmt --check` 通過を確認してコミット

```bash
rtk git add -A
rtk git commit -m "sidebar ヘッダーの dead code と重複ロジックを整理する"
```

## DoD

### 機能完了条件

- [ ] blocked 0 件・working N 件のとき ▲ チップが N 表示・クリック可能で、attn ビューに到達できる(tab サイクル含む)
- [ ] `counts` を持たない旧形式 snapshot を受けてもクライアントが停止せず、counts は全 0 として動作する
- [ ] 購読ストリーム中の不正な1行で購読スレッドが恒久停止しない
- [ ] `header.colors.bg` がモードバッジとアクティブ all チップの両方に効く
- [ ] `badge.glyphs` のカスタムグリフが行バッジ・statusline・ヘッダーチップで一致する
- [ ] `header.suffix` に任意 glyph を設定してもモードバッジ側と総数セグメント側の遷移が対称になる
- [ ] `docs/migration.md` の例をそのままコピーした config がパースエラーにならない
- [ ] 総数 1 件のとき `1 task` と表示される

### テスト完了条件

- [ ] Task 1〜5 の新規テストがすべて通る
- [ ] `rtk cargo test` 全通過、`rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過
- [ ] 削除した `from_rows` / `header_segment_style` / `filter_name` 等への参照が残っていない(`rg` で確認)

### 運用反映条件

- [ ] README のヘッダー仕様(▲ チップの意味、suffix の挙動、chip_prefix/chip_suffix)を更新
- [ ] `docs/migration.md` が現行スキーマと一致
- [ ] `rtk cargo install --path . --force` で再インストールし、`vt sidebar close && vt sidebar open` で表示崩れがないことを目視確認

## スコープ外

- daemon/client の version handshake と stale daemon の自動 respawn(指摘2の根本対策。Task 2 の緩和で実害は解消するため別計画とする)
- 高さ 3 行以下の極小 pane での空状態メッセージ表示(発生条件が非現実的)
- `truncate_display` / `visible_segment_range` の徹底的なアロケーション最適化(描画は ~1Hz で実害なし。Task 5 で自然に減る分のみ)

## 実装ノート

- ユーザーの実環境 config は `~/.config/vde/tmux/config.yml`(dotfiles と同一実体)。`header.colors` は `fg #232332 / bg #98b2f6 / outer_bg #272e42`、`chip_prefix/suffix` は半円キャップ、`bold: false`。回帰確認はこの設定で行う
- ▲ チップの count の意味変更(blocked → attnビュー該当数)により、既存テストの期待値更新が発生する。`✓ done` など他チップの意味は変えない
- Task 1 で述語を共通化する際、`pane_matches_filter` は `AgentPane` を取るが counts 算出も同じ型を走査しているので、述語関数の共有は素直にできるはず
