# Plan 18: Sidebar UI 再設計(Plan 13〜17)レビュー指摘の修正

> **For agentic workers:** 本計画は Task 単位の TDD(失敗テスト → 実装 → 全テスト → コミット)で順番に実行する。チェックボックス(`- [ ]`)で進捗を管理する。

**Goal:** 独立レビュー(2026-07-05 実施)で確定した Critical 1件・Major 7件・Minor 5件を修正し、Plan 13〜17 の成果をマージ可能な状態にする。

**Architecture:** 修正はすべて既存構造(daemon runtime が rows を構築し snapshot を push、TUI は描画とクリック変換のみ)の内側で行う。新しい抽象は Task 8 の `WorkerIo::run_notify` と Task 11 の LIVE capture スレッドのみ。

**Tech Stack:** Rust / ratatui / crossterm / serde / unicode-width(既存依存のみ。新規 crate 追加なし)

## 背景

レビュー結果の指摘 ID(C1, Ma1〜Ma7, Mi1〜Mi5)を本文中で参照する。根拠となる path:line はレビュー時点の HEAD `14249c0` 基準。行番号がズレていても**各 Task に引用したコード断片と意図を正**とすること。

## 実行順序

| 順 | Task | 指摘 | 区分 |
|---|---|---|---|
| 1 | Task 1: 選択の pane 系列判定(テレポート修正) | C1 + Ma7 | **必須** |
| 2 | Task 2: rail の二重計上修正 | Ma1 | **必須** |
| 3 | Task 3: repo ▲N のデバウンス中維持 | Ma3 | **必須** |
| 4 | Task 4: TRIAGE 選択時の出所表示 | Ma2 | **必須** |
| 5 | Task 5: イベントログ表示フォーマット | Ma4 | **必須** |
| 6 | Task 6: DoD チェック更新・未コミット docs の整理 | Ma6 | **必須** |
| 7 | Task 7: ヘッダー hit-test の表示幅化 | Ma5 | 推奨 |
| 8 | Task 8: notify の WorkerIo 化と子プロセス回収 | Mi1, Mi2 | 推奨 |
| 9 | Task 9: 消滅 pane の pin 掃除 | Mi5 | 推奨 |
| 10 | Task 10: daemon 切断時の TUI 終了 | Mi4 | 推奨 |
| 11 | Task 11: LIVE capture の非ブロッキング化 | Mi3 | 推奨 |

Task 1〜6 完了 + ゲート通過でマージ可。Task 7〜11 は同ブランチで続けてよいが、Task 6 までを先に完結させること。

## ルール(Plan 13〜17 と共通)

- 各 Task は TDD: 失敗するテストを書く → 失敗を確認 → 実装 → `rtk cargo test` 全通過 → コミット。
- コミットは Task 単位。メッセージは既存ログの形式(1行目に日本語でやったこと、3行目に箇条書き)。
- 計画とコードの実態が食い違ったら本計画の「意図」を正とし、差分を末尾の「実装ノート」に追記する。
- 設計の再議論はしない。Phase 6 項目(roadmap 参照)は実装しない。
- ゲート: `rtk cargo fmt --check` / `rtk cargo clippy --all-targets` / `rtk cargo test` 全通過。

## DoD(Definition of Done)

### 機能完了条件

- [x] 選択フル展開中の chat 行から `j`/`k` を連打しても選択が先頭/末尾へテレポートしない(jump 行に乗っても展開が維持される)
- [x] TRIAGE⇄FLEET を跨ぐ pane の選択が移動後も追従する
- [x] rail(幅≤2)で選択/展開中でも状態カウント・個別グリフが agent 実数と一致する
- [x] 退出デバウンス保持中(TRIAGE に表示中)の pane が FLEET 側 repo/category 行の ▲N に数え続けられる
- [x] TRIAGE の chat 行を選択すると出所(`category/repo`)が detail 行として表示される
- [x] イベントログが `2m前 codex ● → ▲` 形式(経過時間 + agent 名 + badge glyph + `→`)で表示される
- [x] (Task 7)ヘッダー設定(prefix/suffix/separator)に CJK を含めてもセグメントクリックがずれない
- [x] (Task 8)notify 子プロセスが回収され、テストダブルで発火を検証できる
- [x] (Task 9)消滅した pane の `chat::` pin が state から除去される
- [x] (Task 10)daemon 切断時に TUI が黙って固まらず、終了してエラーメッセージを出す
- [x] (Task 11)tmux 応答遅延時も LIVE 有効のまま TUI の入力・描画が止まらない

### テスト完了条件

- [x] `rtk cargo test` 全通過(本計画の新規テストを含む)
- [x] 新規テスト: 展開 chat 跨ぎの j/k 回帰、ゾーン跨ぎ選択追従、rail 非二重計上、デバウンス中 ▲N、TRIAGE origin detail、event_tail フォーマット、(Task 7 以降)CJK ヘッダー hit-test、notify スパイ、pin 掃除
- [x] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [x] Plan 13〜17 の各計画書 DoD チェックボックスが実態どおり更新されている(Ma6)
- [x] Plan 14 に「選択時 meta 行は Plan 16 でフル展開へ上書き」の実装ノートが追記されている
- [x] Plan 17 の実装ノートに「イベントログ形式の修正」「running pane 存在中は毎秒×全クライアント push となる負荷特性」が追記されている
- [x] `docs/plans/2026-07-05-plan-14-sidebar-ui-phase2.md` と `docs/plans/2026-07-05-sidebar-redesign-roadmap.md` がコミットされている
- [x] `docs/e2e-smoke.md` に Plan 18 の smoke 記録(下記 Task 6 Step 4)が追記されている

---

## Task 1: 選択の pane 系列判定(C1 テレポート修正 + Ma7 ゾーン跨ぎテスト)

**問題:** `src/sidebar/tree.rs` の `push_chat_row` / `triage_zone_rows` は `selected = state.selection == "chat::{id}"` の完全一致で `expanded = selected || manual` を決める。`row_refs`(tree.rs)は `Jump` 行を選択対象に残すため、`j` で選択が `jump::%N` に乗った瞬間、次の rebuild で chat が非選択扱いになり detail/jump 行ごと消える。選択 ID が rows に存在しなくなり、`move_selection`(`src/sidebar/state.rs` の `(None, Next) => 0`)で次の `j` が先頭へ飛ぶ。

**方針:** 展開判定を「selection が同一 pane の `chat::` / `detail::` / `jump::` 系列を指しているか」に緩める。ハイライト(render の `state.selection == row.id` 比較)は変更しない。

**Files:**
- Modify: `src/sidebar/tree.rs`(`push_chat_row`、`triage_zone_rows`、helper 追加)
- Test: `src/sidebar/tree.rs`(mod tests)、`src/daemon/runtime.rs`(mod tests)

- [x] **Step 1: 失敗するテストを書く(tree.rs)**

```rust
#[test]
fn selection_on_child_row_keeps_chat_expanded() {
    let p = pane("main", "%1", "/tmp/app", "codex", "running");
    let state = SidebarState {
        view_mode: ViewMode::Flat,
        selection: Some("jump::%1".to_string()),
        ..SidebarState::default()
    };
    let rows = build_rows_ctx(
        &Config::default(),
        &[p],
        &state,
        &RowBuildContext::default(),
    );
    // 選択が jump 行にあっても親 chat の展開(detail/jump 行)が維持される
    assert!(rows.iter().any(|row| row.id == "jump::%1"));
}
```

- [x] **Step 2: 失敗するテストを書く(runtime.rs、テレポート回帰 + ゾーン跨ぎ)**

既存の `key` クロージャ(`attention_navigation_cycles_blocked_chat_rows` 内と同形)を使う。

```rust
#[test]
fn moving_through_expanded_chat_does_not_teleport_selection() {
    let mut state = RuntimeState::new(
        Config::default(),
        SidebarState {
            view_mode: crate::sidebar::state::ViewMode::Flat,
            ..SidebarState::default()
        },
    );
    state.apply_event(DaemonEvent::PanesUpdated(vec![
        agent_pane("main", "%1", "running"),
        agent_pane("main", "%2", "running"),
    ]));
    let key = |state: &mut RuntimeState, key: &str| {
        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key { key: key.to_string() },
        });
    };
    key(&mut state, "j");
    assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
    key(&mut state, "j"); // フル展開中の chat の jump 行へ移る
    assert_eq!(state.ui_state.selection.as_deref(), Some("jump::%1"));
    assert!(state.rows.iter().any(|row| row.id == "jump::%1"));
    key(&mut state, "j"); // 先頭へテレポートせず次の chat へ進む
    assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%2"));
}

#[test]
fn selection_follows_pane_across_triage_and_fleet() {
    let mut state = RuntimeState::new(
        Config::default(),
        SidebarState {
            view_mode: crate::sidebar::state::ViewMode::Flat,
            ..SidebarState::default()
        },
    );
    let mut blocked = agent_pane("main", "%1", "waiting");
    blocked.wait_reason = "permission_prompt".to_string();
    state.apply_event(DaemonEvent::PanesUpdated(vec![
        blocked,
        agent_pane("main", "%2", "running"),
    ]));
    let key = |state: &mut RuntimeState, key: &str| {
        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key { key: key.to_string() },
        });
    };
    key(&mut state, "n"); // TRIAGE 内の blocked を選択
    assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
    // blocked 解除 → 2回の calm ポーリングで FLEET へ復帰
    for _ in 0..2 {
        state.apply_event(DaemonEvent::PanesUpdated(vec![
            agent_pane("main", "%1", "running"),
            agent_pane("main", "%2", "running"),
        ]));
    }
    // 選択が消えず、復帰後の行として存続している
    assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
    assert!(state.rows.iter().any(|row| row.id == "chat::%1"));
}
```

- [x] **Step 3: テストが失敗することを確認**

Run: `rtk cargo test selection_on_child_row moving_through_expanded selection_follows_pane`
Expected: `selection_on_child_row_keeps_chat_expanded` と `moving_through_expanded_chat_does_not_teleport_selection` が FAIL(ゾーン跨ぎは現状でも通る可能性がある。通った場合も回帰ガードとして残す)。

- [x] **Step 4: 実装(tree.rs)**

helper を追加:

```rust
fn selection_pane_id(selection: Option<&str>) -> Option<&str> {
    let selection = selection?;
    let rest = selection
        .strip_prefix("chat::")
        .or_else(|| selection.strip_prefix("jump::"))
        .or_else(|| selection.strip_prefix("detail::"))?;
    Some(rest.split("::").next().unwrap_or(rest))
}
```

`push_chat_row` と `triage_zone_rows` の両方で:

```rust
// 変更前
let selected = state.selection.as_deref() == Some(id.as_str());
...
let expanded = selected || manual;

// 変更後
let selected =
    selection_pane_id(state.selection.as_deref()) == Some(pane.pane_id.as_str());
let expanded = selected || manual;
```

- [x] **Step 5: 全テスト通過を確認**

Run: `rtk cargo test`
Expected: PASS。既存テストが「選択が子行に移ると展開が畳まれる」ことを前提にしていた場合は、本 Task の仕様(系列選択中は展開維持)に合わせて期待値を更新し、実装ノートに記録する。

- [x] **Step 6: コミット**

```bash
rtk git add src/sidebar/tree.rs src/daemon/runtime.rs
rtk git commit -m "選択中 chat の展開を pane 系列選択で維持する"
```

---

## Task 2: rail の二重計上修正(Ma1)

**問題:** `src/sidebar/render.rs` の `render_rail_lines` が `Chat | Jump` を集計・描画対象にしている。Jump 行は `badge_state: Some(..)` を持つため、選択/展開中の pane がカウントとグリフの両方で二重になる。dense 実装(`Detail | Jump => None`)と不整合。

**Files:**
- Modify: `src/sidebar/render.rs`(`render_rail_lines` のフィルタ)
- Test: `src/sidebar/render.rs`(mod tests)

- [x] **Step 1: 失敗するテストを書く**

`render_rows` は public(once.rs が使用)。既存 rail テスト(`rail_renders_counts_then_rows`)の行フィクスチャ形式に合わせて、同一 pane の Chat 行 + Jump 行を渡す:

```rust
#[test]
fn rail_does_not_double_count_expanded_chat() {
    let chat = SidebarRow {
        id: "chat::%1".to_string(),
        kind: SidebarRowKind::Chat,
        depth: 1,
        label: "codex".to_string(),
        chat_count: 1,
        rollup: RollupLevel::Running,
        badge_state: Some(BadgeState::Working),
        expanded: true,
        pane_id: Some("%1".to_string()),
        git: None,
        meta: None,
    };
    let jump = SidebarRow {
        id: "jump::%1".to_string(),
        kind: SidebarRowKind::Jump,
        depth: 2,
        label: "jump".to_string(),
        chat_count: 0,
        rollup: RollupLevel::Running,
        badge_state: Some(BadgeState::Working),
        expanded: true,
        pane_id: Some("%1".to_string()),
        git: None,
        meta: None,
    };
    let text = render_rows(&[chat, jump], &SidebarState::default(), 2);
    // カウント ●1(●2 ではない)、個別グリフは1行のみ
    assert_eq!(text, "●1\n──\n●");
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test rail_does_not_double_count`
Expected: FAIL(現状は `●2` と2つのグリフ行)。

- [x] **Step 3: 実装**

```rust
// render_rail_lines 内、変更前
.filter(|(_, row)| matches!(row.kind, SidebarRowKind::Chat | SidebarRowKind::Jump))
// 変更後
.filter(|(_, row)| matches!(row.kind, SidebarRowKind::Chat))
```

- [x] **Step 4: 全テスト通過を確認**

Run: `rtk cargo test`
Expected: PASS

- [x] **Step 5: コミット**

```bash
rtk git add src/sidebar/render.rs
rtk git commit -m "rail の集計と描画を Chat 行のみに絞る"
```

---

## Task 3: repo ▲N のデバウンス中維持(Ma3)

**問題:** `src/sidebar/tree.rs` の `group_meta` が `badge_state == BadgeState::Blocked` のみでカウントする。退出デバウンス保持中(TRIAGE には残るが badge はもう Blocked でない)の pane が FLEET 側 ▲N から即座に外れ、デバウンスの意図(ちらつき防止)と矛盾する。

**Files:**
- Modify: `src/sidebar/tree.rs`(`group_meta` と呼び出し元)
- Test: `src/sidebar/tree.rs`(mod tests)

- [x] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn repo_attention_count_keeps_triaged_pane_during_debounce() {
    // %1 は badge がもう Blocked ではないが、退出デバウンスで triage に残っている
    let calm = pane("main", "%1", "/tmp/app", "codex", "running");
    let running = pane("main", "%2", "/tmp/app", "claude", "running");
    let state = SidebarState {
        view_mode: ViewMode::ByRepo,
        ..SidebarState::default()
    };
    let ctx = RowBuildContext {
        triage: BTreeSet::from(["%1".to_string()]),
        now: 1000,
        ..RowBuildContext::default()
    };
    let rows = build_rows_ctx(&Config::default(), &[calm, running], &state, &ctx);
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

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test repo_attention_count_keeps_triaged_pane`
Expected: FAIL(現状は `Some(0)`)。

- [x] **Step 3: 実装**

```rust
// 変更前
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
// 変更後: triage メンバーシップも数える
fn group_meta(panes: &[AgentPane], triage: &BTreeSet<String>) -> RowMeta {
    RowMeta {
        attention_count: Some(
            panes
                .iter()
                .filter(|pane| {
                    pane.badge_state == BadgeState::Blocked
                        || triage.contains(&pane.pane_id)
                })
                .count(),
        ),
        ..RowMeta::default()
    }
}
```

呼び出し元(`group_metas` の構築。triage drain より前にある):

```rust
let group_metas = groups
    .iter()
    .map(|(key, panes)| (key.clone(), group_meta(panes, &ctx.triage)))
    .collect::<BTreeMap<_, _>>();
```

- [x] **Step 4: 全テスト通過を確認**

Run: `rtk cargo test`
Expected: PASS(既存 `repo_attention_count_includes_triaged_panes` も通ること)。

- [x] **Step 5: コミット**

```bash
rtk git add src/sidebar/tree.rs
rtk git commit -m "repo の attention 件数に triage 保持中の pane を含める"
```

---

## Task 4: TRIAGE 選択時の出所表示(Ma2)

**問題:** `triage_zone_rows` で origin(`category/repo`)は `else if pinned` 分岐の `push_meta_row(.., Some(&origin), ..)` でしか出ない。選択(=フル展開)時は `push_chat_detail_rows`(origin 情報なし)になり、Plan 15 DoD「TRIAGE 行を選択すると出所が含まれる」が満たされない。

**方針:** 展開時に origin の detail 行を1行追加する(Plan 15 の文言は「inline meta 行に含まれる」だが、Plan 16 で選択時はフル展開に変わったため、detail 行として表示する。実装ノートに読み替えを記録すること)。

**Files:**
- Modify: `src/sidebar/tree.rs`(`triage_zone_rows` の expanded 分岐)
- Test: `src/sidebar/tree.rs`(mod tests)

- [x] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn selected_triage_row_shows_origin_detail() {
    let mut blocked = pane("main", "%1", "/tmp/app", "codex", "waiting");
    blocked.wait_reason = "permission_prompt".to_string();
    let state = SidebarState {
        selection: Some("chat::%1".to_string()),
        ..SidebarState::default()
    };
    let ctx = RowBuildContext {
        triage: BTreeSet::from(["%1".to_string()]),
        now: 1000,
        ..RowBuildContext::default()
    };
    let rows = build_rows_ctx(&Config::default(), &[blocked], &state, &ctx);
    let origin_row = rows
        .iter()
        .find(|row| row.id == "detail::%1::origin")
        .expect("origin detail row");
    assert!(origin_row.label.contains("misc/app"));
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test selected_triage_row_shows_origin`
Expected: FAIL(`origin detail row` が見つからない)。

- [x] **Step 3: 実装**

`triage_zone_rows` の展開分岐に origin detail 行を追加:

```rust
if expanded {
    rows.push(detail_row(pane, 2, "origin", format!("origin: {origin}")));
    push_chat_detail_rows(pane, 2, now, &mut rows);
} else if pinned {
    push_meta_row(pane, 2, now, Some(&origin), &mut rows);
}
```

- [x] **Step 4: 全テスト通過を確認**

Run: `rtk cargo test`
Expected: PASS(既存 `triage_rows_carry_origin_in_meta` は pin 経路のテストとしてそのまま残す)。

- [x] **Step 5: コミット**

```bash
rtk git add src/sidebar/tree.rs
rtk git commit -m "TRIAGE 選択時に出所の detail 行を表示する"
```

---

## Task 5: イベントログ表示フォーマット(Ma4)

**問題:** `src/sidebar/tui.rs` の `event_tail` が `"{pane_id} {from:?} -> {to:?}"`(例 `%1 Working -> Blocked`)を出力しており、Plan 17 DoD の `2m前 codex ● → ▲` と不一致。`TransitionEvent.at_epoch` と `agent` が未使用、glyph 未使用、矢印が ASCII。

**Files:**
- Modify: `src/sidebar/tui.rs`(`event_tail`、`render_live_lines` とその呼び出し元)
- Modify: `src/sidebar/render.rs`(`badge_glyph` を `pub(crate)` にする)
- Test: `src/sidebar/tui.rs`(mod tests)

- [x] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn event_tail_formats_ago_agent_and_glyphs() {
    let mut snapshot = crate::daemon::build_snapshot_with_sidebar(&[], None);
    snapshot.events.push(crate::daemon::TransitionEvent {
        pane_id: "%1".to_string(),
        agent: "codex".to_string(),
        from: Some(crate::daemon::session_badge::BadgeState::Working),
        to: crate::daemon::session_badge::BadgeState::Blocked,
        at_epoch: 880,
    });
    let theme = SidebarRenderTheme::from_app_config(&Config::default());
    let lines = event_tail(&snapshot, 3, 1000, &theme);
    assert_eq!(lines, vec!["2m前 codex ● → ▲".to_string()]);
}
```

(import は既存 tui テストの use に合わせて調整する。`BadgeState` の定義は `src/daemon/session_badge.rs`。)

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test event_tail_formats_ago`
Expected: FAIL(コンパイルエラー: 引数不一致。または旧フォーマット)。

- [x] **Step 3: 実装**

`src/sidebar/render.rs`: `fn badge_glyph(&self, state: BadgeState) -> &str` を `pub(crate) fn` に変更。

`src/sidebar/tui.rs`:

```rust
fn event_tail(
    snapshot: &DaemonSnapshot,
    limit: usize,
    now: i64,
    theme: &SidebarRenderTheme,
) -> Vec<String> {
    let mut events = snapshot
        .events
        .iter()
        .rev()
        .take(limit)
        .map(|event| {
            let elapsed = (now - event.at_epoch).max(0);
            let ago = if elapsed >= 60 {
                format!("{}m前", elapsed / 60)
            } else {
                format!("{elapsed}s前")
            };
            let from = event
                .from
                .map(|state| theme.badge_glyph(state).to_string())
                .unwrap_or_else(|| "·".to_string());
            format!(
                "{ago} {} {} → {}",
                event.agent,
                from,
                theme.badge_glyph(event.to)
            )
        })
        .collect::<Vec<_>>();
    events.reverse();
    events
}
```

`render_live_lines` に `now: i64, theme: &SidebarRenderTheme` を追加し、`LiveMode::Events => event_tail(snapshot, body_limit, now, theme)` とする。呼び出し元(main loop)では `theme` は `RunLoopConfig` 経由で既にスコープにあり、`now` は `crate::sidebar::tree::now_epoch_secs()` を渡す。

- [x] **Step 4: 全テスト通過を確認**

Run: `rtk cargo test`
Expected: PASS

- [x] **Step 5: コミット**

```bash
rtk git add src/sidebar/tui.rs src/sidebar/render.rs
rtk git commit -m "イベントログを経過時間と glyph 付きの仕様形式にする"
```

---

## Task 6: DoD チェック更新・未コミット docs の整理(Ma6)

コード変更なし。ドキュメントの整合のみ。

**Files:**
- Modify: `docs/plans/2026-07-05-plan-13〜17-sidebar-ui-phase*.md`
- Modify: `docs/e2e-smoke.md`
- Commit(新規追跡): `docs/plans/2026-07-05-plan-14-sidebar-ui-phase2.md`、`docs/plans/2026-07-05-sidebar-redesign-roadmap.md`

- [x] **Step 1: 各 Plan の DoD チェックボックスを実態に合わせて更新**

- Plan 13: 全項目 `[x]`(ヘッダー hit-test は「既定 ASCII 設定で機能。CJK 設定は Plan 18 Task 7 で対応」と注記)
- Plan 14: 全項目 `[x]`。実装ノート節を新設し「DoD 1・2(選択時 inline meta)は Plan 16 の3段階行高でフル展開仕様に上書きされた。現仕様では meta 1行は pin 時のみ」と追記
- Plan 15: Task 3・4 の修正を根拠に DoD 4・5 を `[x]`。テスト完了条件「ゾーン跨ぎ選択移動」は Task 1 の `selection_follows_pane_across_triage_and_fleet` を根拠に `[x]`
- Plan 16: Task 1・2 の修正を根拠に全項目 `[x]`。実装ノートに「選択展開は pane 系列判定に修正(Plan 18)」を追記
- Plan 17: Task 5 の修正を根拠に全項目 `[x]`。実装ノートに「event log 形式は Plan 18 で仕様形式に修正」「running pane が存在する間は fingerprint が毎秒変化し全クライアントへ毎秒 push される(意図的設計)」を追記
- roadmap の共通ゲート・全体 DoD チェックボックスも実態に合わせて更新

- [x] **Step 2: Task 順のコミットを確認し、docs をコミット**

```bash
rtk git add docs/plans/ docs/e2e-smoke.md
rtk git commit -m "Plan 13〜17 の DoD チェックと実装ノートを実態に合わせる"
```

(plan-14 と roadmap はこのコミットで初めて追跡される。`docs/sidebar-ui-proposals.html` / `docs/statusline-ui-proposals.*` は本計画のスコープ外なので add しない。)

- [x] **Step 3: 品質ゲート**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`
Expected: すべて通過。

- [x] **Step 4: scratch tmux smoke(Task 1〜5 の実機確認)**

`docs/e2e-smoke.md` の共通準備どおり scratch server + fake agent バイナリ(実行ファイル名が `codex`/`claude` である必要あり。`sleep` の症状回避のため C で作る手順が確立済み)で:

1. running 2 pane を作り、TUI で chat 選択 → `j` 連打で選択が先頭へ飛ばないこと
2. permission blocked を作り、TRIAGE 行選択で `origin:` detail 行が出ること
3. 承認後、2ポーリングの間 repo 行 ▲N が維持されること(daemon subscribe snapshot の `attention_count` で確認可)
4. 幅2で選択中でもカウントが実数と一致すること(`attach --once` は runtime 状態を持たないため、TUI か subscribe snapshot で確認)
5. `e` でイベントログが `Xs前/Xm前 {agent} {glyph} → {glyph}` 形式で出ること

結果を `docs/e2e-smoke.md` に Plan 18 として追記し、daemon 再起動が必要な旨を記録。

- [x] **Step 5: コミット**

```bash
rtk git add docs/e2e-smoke.md
rtk git commit -m "Plan 18 の smoke 結果を記録する"
```

**ここまででマージ可。以降は推奨タスク。**

---

## Task 7: ヘッダー hit-test の表示幅化(Ma5)

**問題:** `build_header_layout_with_counts` がセグメント range を `chars().count()`(文字数)で計算し、マウスの `column`(表示セル)と比較している。`render_header_lines` も `slice_chars`(文字数スライス)で range を使う。既定 ASCII では一致するが、`sidebar.header.prefix/suffix/separator/format` に全角を入れるとクリック位置と描画スタイルの両方がずれる。

**方針:** range の単位を「表示セル」に統一する。長さ計算を `display_width`、スライスを表示幅ベースの `slice_display` に置き換える。

**Files:**
- Modify: `src/sidebar/render.rs`
- Test: `src/sidebar/render.rs`(mod tests)

- [x] **Step 1: 失敗するテストを書く**

既存の `header_layout_can_be_configured_as_pill_buttons` を参考に、prefix に全角を入れた設定でセグメント開始位置を検証する:

```rust
#[test]
fn header_segments_use_display_cells_with_cjk_prefix() {
    // theme の header_prefix を "「"(表示幅2)にした設定を用意する
    // (既存テストの設定手段に合わせて SidebarRenderTheme を構築する)
    let mut config = Config::default();
    config.sidebar.header.prefix = "「".to_string();
    config.sidebar.header.suffix = "」".to_string();
    let theme = SidebarRenderTheme::from_app_config(&config);
    let state = SidebarState::default();
    let layout = build_header_layout_with_counts(&state, 60, &theme, BadgeCounts::default());
    let line = &layout.lines[0];
    // mode セグメント: 左 padding 1 + prefix(幅2)を含むラベル。
    // range が文字数(「=1)ではなく表示セル(「=2)で数えられていることを検証する
    let mode = &line.segments[0];
    let mode_text = format_header_segment(view_mode_label(state.view_mode), &theme);
    assert_eq!(
        (mode.range.end - mode.range.start) as usize,
        display_width(&mode_text)
    );
    // 描画側も同じ range でずれない(先頭セグメントのスライスがラベル全体と一致)
    let lines = render_header_lines(&layout, &theme);
    assert!(lines[0].spans.iter().any(|span| span.content == mode_text));
}
```

(`config.sidebar.header` のフィールド名は `src/config/schema.rs` の実名に合わせる。`BadgeCounts::default()` が無ければ 0 埋めのコンストラクタを使う。)

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test header_segments_use_display_cells`
Expected: FAIL(range が文字数基準)。

- [x] **Step 3: 実装**

1. `slice_display` を追加(表示セル区間 [start, end) に対応する部分文字列。幅2文字が境界を跨ぐ場合は含めない):

```rust
fn slice_display(text: &str, start: u16, end: u16) -> String {
    let mut cell = 0_u16;
    let mut out = String::new();
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if cell >= end {
            break;
        }
        if cell >= start && cell + w <= end {
            out.push(ch);
        }
        cell += w;
    }
    out
}
```

2. `build_header_layout_with_counts` 内の `mode_badge.chars().count()` / `separator.chars().count()` / `label.chars().count()` をすべて `display_width(..)` に置換。
3. `visible_segment_range` の `text.chars().count()` を `display_width(text)` に置換。
4. `render_header_lines` の `slice_chars` 呼び出し3箇所を `slice_display` に、`line.text.chars().count()` を `display_width(&line.text)` に置換。`slice_chars` が他で未使用になったら削除する。

- [x] **Step 4: 全テスト通過を確認**

Run: `rtk cargo test`
Expected: PASS(ASCII 既定ではセル数=文字数なので既存 hit-test テストは変化しない)。

- [x] **Step 5: コミット**

```bash
rtk git add src/sidebar/render.rs
rtk git commit -m "ヘッダーのセグメント範囲を表示セル基準に統一する"
```

---

## Task 8: notify の WorkerIo 化と子プロセス回収(Mi1, Mi2)

**問題:** `src/daemon/server.rs` の `RuntimeEffect::Notify` 分岐が `Command::spawn` を直接呼び、(1) テストダブルで検証できない、(2) `Child` を破棄しゾンビが溜まる。

**Files:**
- Modify: `src/daemon/workers.rs`(trait にメソッド追加、`SystemWorkerIo` 実装、`MockWorkerIo` にスパイ追加)
- Modify: `src/daemon/server.rs`(Notify 分岐を委譲、`LoopWorkerIo` にも実装)
- Test: `src/daemon/server.rs`(mod tests)

- [x] **Step 1: 失敗するテストを書く**

`server.rs` の既存テストで使われている fake(`LoopWorkerIo`)に `notify_calls: Mutex<Vec<(String, String, String, String)>>` を追加し、`handle_runtime_effects` に `RuntimeEffect::Notify` を渡すテストを書く:

```rust
#[test]
fn notify_effect_runs_command_via_worker_io() {
    let io = LoopWorkerIo::default();
    handle_runtime_effects(
        vec![RuntimeEffect::Notify {
            pane_id: "%1".to_string(),
            agent: "codex".to_string(),
            state: BadgeState::Blocked,
        }],
        &io,
        Some("true"),
        /* 既存シグネチャの他引数に合わせる */
    )
    .unwrap();
    let calls = io.notify_calls.lock().unwrap();
    assert_eq!(
        calls.as_slice(),
        &[(
            "true".to_string(),
            "%1".to_string(),
            "codex".to_string(),
            "Blocked".to_string()
        )]
    );
}
```

(`handle_runtime_effects` の実シグネチャ(`notify_command` の受け渡し方)に合わせて調整する。)

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test notify_effect_runs_command_via_worker_io`
Expected: FAIL(コンパイルエラー: trait にメソッドが無い)。

- [x] **Step 3: 実装**

`workers.rs` の trait に追加(state は wire に載せない transient のため文字列で受ける。Plan 17 実装ノートの方針を維持):

```rust
pub trait WorkerIo: Send + Sync + 'static {
    // ...既存メソッド...
    fn run_notify(&self, command: &str, pane_id: &str, agent: &str, state: &str) -> Result<()>;
}
```

`SystemWorkerIo` 実装(spawn + 別スレッドで wait して回収):

```rust
fn run_notify(&self, command: &str, pane_id: &str, agent: &str, state: &str) -> Result<()> {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("VDE_PANE_ID", pane_id)
        .env("VDE_AGENT", agent)
        .env("VDE_BADGE_STATE", state)
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
```

`server.rs` の Notify 分岐:

```rust
RuntimeEffect::Notify { pane_id, agent, state } => {
    if let Some(command) = notify_command
        && let Err(error) =
            worker_io.run_notify(command, &pane_id, &agent, &format!("{state:?}"))
    {
        eprintln!("[vde-tmux] notify command failed: {error:#}");
    }
}
```

`MockWorkerIo`(workers.rs)と `LoopWorkerIo`(server.rs)にもスパイ実装を追加する。

- [x] **Step 4: 全テスト通過を確認**

Run: `rtk cargo test`
Expected: PASS

- [x] **Step 5: コミット**

```bash
rtk git add src/daemon/workers.rs src/daemon/server.rs
rtk git commit -m "notify 実行を WorkerIo 経由にして子プロセスを回収する"
```

---

## Task 9: 消滅 pane の pin 掃除(Mi5)

**問題:** pin した pane が閉じても `ui_state.pinned` の `chat::%N` が残留し、`state.json` に永続化され続ける。

**Files:**
- Modify: `src/daemon/runtime.rs`(`PanesUpdated` 分岐)
- Test: `src/daemon/runtime.rs`(mod tests)

- [x] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn pinned_entries_for_missing_panes_are_pruned() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    state
        .ui_state
        .pinned
        .insert("chat::%9".to_string());
    state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    assert!(!state.ui_state.pinned.contains("chat::%9"));
    // 現存 pane の pin は消さない
    state.ui_state.pinned.insert("chat::%1".to_string());
    state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    assert!(state.ui_state.pinned.contains("chat::%1"));
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test pinned_entries_for_missing_panes`
Expected: FAIL(1つ目の assert)。

- [x] **Step 3: 実装**

`PanesUpdated` 分岐(`self.panes = panes;` 直後、`update_unread()` の前)に:

```rust
let live_chat_ids: std::collections::BTreeSet<String> = self
    .panes
    .iter()
    .map(|pane| format!("chat::{}", pane.pane_id))
    .collect();
self.ui_state
    .pinned
    .retain(|id| live_chat_ids.contains(id));
```

(`collapsed` は repo/category の折りたたみキーを含むため掃除しない。)

- [x] **Step 4: 全テスト通過を確認**

Run: `rtk cargo test`
Expected: PASS

- [x] **Step 5: コミット**

```bash
rtk git add src/daemon/runtime.rs
rtk git commit -m "消滅した pane の pin を PanesUpdated で掃除する"
```

---

## Task 10: daemon 切断時の TUI 終了(Mi4)

**問題:** subscribe スレッド(`src/sidebar/client.rs`)は decode 失敗や daemon 停止で `break` して sender を drop するが、TUI main loop は `while let Ok(snapshot) = rx.try_recv()` で Empty と Disconnected を区別しないため、以後更新の止まった画面のまま黙って固まる。

**Files:**
- Modify: `src/sidebar/tui.rs`(main loop の受信部、`TuiExit`、`attach` 側の後処理)
- Test: 手動確認(smoke)。チャネル切断の単体テストは `run_loop` が terminal を要求するため対象外とし、実装ノートに記録する

- [x] **Step 1: 実装**

`TuiExit` に variant を追加:

```rust
pub enum TuiExit {
    Quit,
    Disconnected,
}
```

main loop の受信部を置き換え:

```rust
// 変更前
while let Ok(snapshot) = rx.try_recv() {
    current = Some(snapshot);
}
// 変更後
loop {
    match rx.try_recv() {
        Ok(snapshot) => current = Some(snapshot),
        Err(std::sync::mpsc::TryRecvError::Empty) => break,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            return Ok(TuiExit::Disconnected);
        }
    }
}
```

呼び出し元(`attach`、raw mode 解除後):

```rust
match result? {
    TuiExit::Quit => spawn_detached_sidebar_close(&std::env::current_exe()?, &close_window)?,
    TuiExit::Disconnected => {
        eprintln!("[vde-tmux] daemon への接続が終了しました。daemon を再起動して attach し直してください。");
    }
}
```

`TuiExit` の比較箇所(`if result? == TuiExit::Quit`)は match に置き換える。

- [x] **Step 2: 全テスト通過を確認してコミット**

Run: `rtk cargo test && rtk cargo clippy --all-targets`
Expected: PASS

```bash
rtk git add src/sidebar/tui.rs
rtk git commit -m "daemon 切断時に TUI をメッセージ付きで終了する"
```

---

## Task 11: LIVE capture の非ブロッキング化(Mi3)

**問題:** `update_live_state` が main loop 内で同期的に `capture-pane` を実行する。runner の timeout は 1秒(`SystemTmuxRunner::from_env(Duration::from_secs(1))`)で、tmux が遅延すると入力・描画が最大1秒停止する。

**方針:** capture 専用スレッドを1本立て、要求(pane_id)と結果(pane_id, lines)をチャネルで往復する。main loop は送信と `try_recv` のみ。

**Files:**
- Modify: `src/sidebar/tui.rs`(`LiveState` 周辺、`update_live_state`)
- Test: 既存の `extract_tail` / `compute_areas` テスト維持 + 手動 smoke。スレッド往復の単体テストは追加しない(実 tmux 依存のため)。実装ノートに記録する

- [x] **Step 1: 実装**

capture スレッドの起動(`run_loop` 呼び出し前、`attach` 内):

```rust
fn spawn_live_capture_worker(
    request_rx: std::sync::mpsc::Receiver<String>,
    result_tx: std::sync::mpsc::Sender<(String, String)>,
) {
    std::thread::spawn(move || {
        let runner = SystemTmuxRunner::from_env(Duration::from_millis(500));
        while let Ok(pane_id) = request_rx.recv() {
            let output = runner
                .run(&["capture-pane", "-p", "-t", &pane_id])
                .unwrap_or_default();
            if result_tx.send((pane_id, output)).is_err() {
                break;
            }
        }
    });
}
```

(`capture-pane` の実引数は現行 `update_live_state` が渡しているものをそのまま移す。)

`update_live_state` の変更点:

- interval 経過判定・pane 切替時のクリアは現状維持
- 同期 `runner.run(..)` を `request_tx.send(pane_id)` に置き換え(未処理要求がある間は再送しないよう `live.capture_in_flight: bool` を `LiveState` に追加)
- 毎 tick で `result_rx.try_recv()` を回し、`live.pane_id` と一致する結果のみ `live.lines = extract_tail(&output, ..)` に反映して `capture_in_flight = false`

- [x] **Step 2: 全テスト・ゲート通過を確認**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`
Expected: すべて通過。

- [x] **Step 3: scratch tmux で LIVE の実機確認**

scratch daemon + TUI で LIVE が従来どおり更新されること、`e` トグル・`sidebar.live.enabled: false` が機能することを確認し、`docs/e2e-smoke.md` の Plan 18 記録に追記する。

- [x] **Step 4: コミット**

```bash
rtk git add src/sidebar/tui.rs docs/e2e-smoke.md
rtk git commit -m "LIVE capture を専用スレッドに逃がして TUI を非ブロッキングにする"
```

---

## スコープ外(実装しないこと)

- 単一 pane repo が TRIAGE 移動で FLEET から消える挙動の変更(レビューでグレーゾーンと判定。Phase 6 の観察対象として `docs/sidebar-ui-proposals.md` に追記するのは可)
- dense/micro ティアへの pin 印追加(ティア仕様自体の変更になるため)
- flash の REVERSED 適用範囲のティア間統一
- `SidebarRowKind` への unknown-variant フォールバック(後方互換を作らない方針のため)
- Phase 6 の全項目(roadmap 参照)

## Codex への引き渡しプロンプト(そのまま使用可)

```
docs/plans/2026-07-05-plan-18-sidebar-review-fixes.md を読み、Task 1 から
順番に実装してください。

ルール:
- 各 Task を TDD 手順(失敗テスト → 失敗確認 → 実装 → rtk cargo test 全通過 → コミット)で進める
- Task 1〜6(必須)を先に完結させる。Task 7〜11(推奨)はその後
- 計画中の path:line は 14249c0 時点の参考値。実コードと食い違ったら、各 Task に
  引用されたコード断片と「問題」「方針」の記述を正とし、差分を計画末尾の
  「実装ノート」に追記する
- 「スコープ外」節の項目は実装しない
- 最後に rtk cargo fmt --check / rtk cargo clippy --all-targets / rtk cargo test の
  全通過と、docs/e2e-smoke.md への Plan 18 smoke 記録を確認する
```

## 実装ノート

(実装時に計画からの逸脱があればここに追記する)
