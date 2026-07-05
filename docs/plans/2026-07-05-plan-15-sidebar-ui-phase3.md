# Plan 15: TRIAGE 常設ゾーン + FLEET(UI 再設計 Phase 3)

> **実装者向け:** `docs/sidebar-ui-proposals.md` §9.2 Phase 3 の実装。**Plan 13・14 完了が前提**。F案の骨格となる最重要 Phase。Task 順に実施する。

**Goal:** blocked(badge ▲)の agent を常設 TRIAGE ゾーンとして最上部に分離し、残りを FLEET(従来の ViewMode グルーピング)として表示する。TRIAGE はフィルタを貫通し、0件で自動消滅する。状態フリップによる行のちらつきは退出側デバウンスで抑える。

**Architecture:** ゾーンは「rows 先頭に Zone 見出し行 + TRIAGE chat 行群を挿入する」だけの平坦なリストとして実現する(選択移動・クリックの 1:1 対応は自然に維持される)。TRIAGE メンバーシップは runtime 側で管理し(退出デバウンスに世代カウンタが必要なため)、`BTreeSet<String>` として tree 構築に渡す。build 系関数のパラメータ増殖を止めるため `RowBuildContext` 構造体にまとめるリファクタを先行させる。

**Tech Stack:** Plan 13/14 と同じ(新規依存なし)

## DoD

### 機能完了条件

- [x] blocked な agent が repo ツリーから抜けて最上部の TRIAGE ゾーンに `▲ {agent} · {repo}` + 右端 `perm/wait/err` 形式で表示される
- [x] TRIAGE ゾーン見出しが `▍TRIAGE {N}`(badge_blocked 色・BOLD)で表示され、0件では見出しごと消える
- [x] TRIAGE は StatusFilter(all/attention)の影響を受けない
- [x] FLEET 側の repo 行の右端 `▲N` は TRIAGE に移動した agent も数え続ける(出所の手がかり)
- [x] TRIAGE 行を選択すると inline meta 行(Plan 14)に出所(`category/repo`)が含まれる
- [x] blocked 解除後、2回連続の非 blocked ポーリングを経てから FLEET に戻る(退出デバウンス)。blocked への遷移は即時
- [x] ViewMode(flat/repo/category)・折りたたみ・J/K 並べ替えは FLEET 部分で従来どおり動作する
- [x] `n`/`N`(Plan 14)が折りたたみ状態に関わらず全 blocked を巡回できる(TRIAGE に常時表示されるため)

### テスト完了条件

- [x] `rtk cargo test` 全通過
- [x] 新規テスト: TRIAGE 分離と0件消滅、フィルタ貫通、repo ▲N の維持、退出デバウンス、ゾーン跨ぎ選択移動
- [x] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [x] `docs/e2e-smoke.md` に TRIAGE の確認手順(permission 発生→最上部に出現→承認→2秒後に FLEET へ戻る)を追記し、smoke 実施を記録
- [x] `docs/sidebar-ui-proposals.md` §9.2 Phase 3 にチェック

---

## Task 0: RowBuildContext リファクタ(挙動変更なし)

**Files:**
- Modify: `src/sidebar/tree.rs`(build 系関数のシグネチャ整理)
- Modify: `src/daemon/runtime.rs:211-224`(呼び出し)

- [ ] **Step 1: 実装(リファクタのみ、テストは既存のまま通す)**

`src/sidebar/tree.rs` に追加:

```rust
#[derive(Debug, Clone, Default)]
pub struct RowBuildContext {
    pub git: BTreeMap<String, crate::git::GitBadge>,
    pub unread: BTreeMap<String, bool>,
    pub triage: std::collections::BTreeSet<String>,
    pub now: i64,
}
```

新しいエントリポイントを追加し、既存の `build_rows_with_git_and_unread` / `build_rows_at_with_git_and_unread` はこれを呼ぶ薄い wrapper に変える(既存テスト・呼び出しを壊さない):

```rust
pub fn build_rows_ctx(
    config: &Config,
    panes: &[PaneSnapshot],
    state: &SidebarState,
    ctx: &RowBuildContext,
) -> Vec<SidebarRow> {
    // 現在の build_rows_at_with_git_and_unread の本体をここへ移動。
    // git/unread/now は ctx から読む。ctx.triage は Task 2 まで未使用。
}
```

`src/daemon/runtime.rs` の `rebuild_snapshot`(211-224行)を `build_rows_ctx` 呼び出しに変更(`triage: BTreeSet::new()`、`now` は `now_epoch_secs` 相当を tree 側 wrapper と同じ方法で。tree.rs の `now_epoch_secs` を `pub(crate)` にして使う)。

- [ ] **Step 2: テスト通過を確認してコミット**

Run: `rtk cargo test`
Expected: 全通過(挙動変更なし)

```bash
rtk git add -A
rtk git commit -m "sidebar row 構築の引数を RowBuildContext にまとめる"
```

---

## Task 1: Zone 行種と TRIAGE レンダリング

**Files:**
- Modify: `src/sidebar/tree.rs:13-20`(SidebarRowKind に Zone 追加)、`row_refs` 161-166行
- Modify: `src/sidebar/render.rs`(Zone 行の描画)
- Modify: `src/sidebar/input.rs:52-64`(activate_selected の網羅)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests:

```rust
#[test]
fn zone_row_renders_as_colored_heading() {
    let mut zone = row(
        "zone::triage",
        SidebarRowKind::Zone,
        0,
        "TRIAGE",
        RollupLevel::Permission,
    );
    zone.chat_count = 2;
    let lines = render_lines(
        &[zone],
        &SidebarState::default(),
        30,
        &SidebarRenderTheme::default(),
    );
    let text: String = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.starts_with(" ▍TRIAGE 2"), "{text:?}");
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|span| span.style.fg == Some(Color::Red)
                && span.style.add_modifier.contains(Modifier::BOLD)),
        "{lines:?}"
    );
}
```

`src/sidebar/tree.rs` tests:

```rust
#[test]
fn row_refs_exclude_zone_rows() {
    // Zone 行を含む rows を作り、row_refs に zone:: が含まれないことを検証
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render::tests::zone_row_renders_as_colored_heading`
Expected: コンパイルエラー(Zone variant 未定義)

- [ ] **Step 3: 実装**

`src/sidebar/tree.rs`:
- `SidebarRowKind` に `Zone` を追加(serde derive は既存のまま)
- `row_refs`(161-166行)のフィルタを `row.kind != SidebarRowKind::Detail && row.kind != SidebarRowKind::Zone` に変更

`src/sidebar/render.rs` の `render_row_line` の `head` match に Zone 分岐を追加し、Zone は専用の組み立てにする(right/badge/git なし):

```rust
    if row.kind == SidebarRowKind::Zone {
        let text = truncate_display(
            &format!(" ▍{} {}", row.label, row.chat_count),
            width.saturating_sub(1),
        );
        let style = Style::default()
            .fg(theme.badge_color(BadgeState::Blocked))
            .add_modifier(Modifier::BOLD);
        return Line::from(Span::styled(text, style));
    }
```

(`render_row_line` 冒頭、selected 判定の直後に入れる。Zone は選択対象外なので bg 処理不要。)

`row_style` / `right_label` の match に `SidebarRowKind::Zone => ...` を追加して網羅性エラーを解消(`row_style` は到達しないが `Style::default()` を返す。`right_label` は `None`)。

`src/sidebar/input.rs` の `activate_selected` match に `SidebarRowKind::Zone => None,` を追加。

`src/sidebar/tui.rs` の `single_click_action`(286-294行)に `SidebarRowKind::Zone => None,` を追加。

rail(`render_rail_lines`)は Chat/Jump のみ対象なので変更不要。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "sidebar に Zone 行種を追加する"
```

---

## Task 2: TRIAGE 分離の行構築

**Files:**
- Modify: `src/sidebar/tree.rs`(`build_rows_ctx` 本体、RowMeta.origin 追加)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/tree.rs` tests:

```rust
#[test]
fn blocked_panes_move_to_triage_zone_on_top() {
    // blocked(permission)1体 + running 1体を同一 repo に置き、
    // ctx.triage = {"%1"} で build_rows_ctx(ByRepo)
    let rows = /* ... */;
    assert_eq!(rows[0].id, "zone::triage");
    assert_eq!(rows[0].chat_count, 1);
    assert_eq!(rows[1].id, "chat::%1");
    assert_eq!(rows[1].depth, 1);
    // TRIAGE 行のラベルは "{agent} · {repo}"
    assert_eq!(rows[1].label, "codex · app");
    // FLEET 側(repo 行以降)に %1 の chat 行が存在しない
    assert!(!rows[2..].iter().any(|row| row.id == "chat::%1"));
}

#[test]
fn triage_zone_is_absent_when_empty() {
    // ctx.triage 空 → rows に zone:: が無い
}

#[test]
fn triage_ignores_attention_filter() {
    // filter=AttentionOnly かつ ctx.triage={"%1"}(blocked)、
    // 他に idle 1体 → TRIAGE に %1 が残り、idle は FLEET から消える
}

#[test]
fn repo_attention_count_includes_triaged_panes() {
    // blocked 1体が TRIAGE に移動しても、その repo 行の
    // meta.attention_count == Some(1) のまま
}

#[test]
fn triage_rows_carry_origin_in_meta() {
    // TRIAGE 行の meta.origin == Some("misc/app")
    // かつ選択時の inline meta 行 label に "misc/app" が含まれる
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::tree::tests::blocked_panes_move_to_triage_zone_on_top`
Expected: FAIL

- [ ] **Step 3: 実装**

`RowMeta`(Plan 13 で追加済み)にフィールド追加:

```rust
    pub origin: Option<String>,
```

`build_rows_ctx` の本体を次の順序に再構成する:

```rust
    // 1) 全 pane を groups に収集(従来どおり、フィルタ前)
    // 2) 各グループの group_meta(attention_count)を BTreeMap<(cat,repo), RowMeta>
    //    としてここで先に計算する(TRIAGE 移動前の姿で数える)
    // 3) TRIAGE 抽出: ctx.triage に含まれる pane_id を groups から drain する
    let mut triage_panes: Vec<AgentPane> = Vec::new();
    for panes in groups.values_mut() {
        let mut index = 0;
        while index < panes.len() {
            if ctx.triage.contains(&panes[index].pane_id) {
                triage_panes.push(panes.remove(index));
            } else {
                index += 1;
            }
        }
    }
    triage_panes.sort_by(compare_agent_panes);
    // 4) StatusFilter を FLEET 側にのみ適用(従来の retain)
    //    AttentionOnly は pane.attention || rollup == Running に変更
    //    (Error/Permission/Waiting は TRIAGE 側に居るため)
    // 5) rows 組み立て: triage_zone_rows(&triage_panes, state, ctx.now)
    //    ++ 従来の view_mode 分岐(repo 行の meta は 2) の事前計算値を使う)
```

TRIAGE 行の構築:

```rust
fn triage_zone_rows(panes: &[AgentPane], state: &SidebarState, now: i64) -> Vec<SidebarRow> {
    if panes.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![SidebarRow {
        id: "zone::triage".to_string(),
        kind: SidebarRowKind::Zone,
        depth: 0,
        label: "TRIAGE".to_string(),
        chat_count: panes.len(),
        rollup: rollup(panes),
        badge_state: badge_rollup(panes),
        expanded: true,
        pane_id: None,
        git: None,
        meta: None,
    }];
    for pane in panes {
        let id = format!("chat::{}", pane.pane_id);
        let selected = state.selection.as_deref() == Some(id.as_str());
        let mut meta = chat_meta(pane, now);
        meta.origin = Some(format!("{}/{}", pane.category, pane.repo));
        rows.push(SidebarRow {
            id: id.clone(),
            kind: SidebarRowKind::Chat,
            depth: 1,
            label: format!("{} · {}", pane.agent, pane.repo),
            chat_count: 1,
            rollup: pane.rollup,
            badge_state: Some(pane.badge_state),
            expanded: false,
            pane_id: Some(pane.pane_id.clone()),
            git: None,
            meta: Some(meta),
        });
        if selected {
            // Plan 14 の meta 行と同形式(meta_label に origin を先頭追加)
            rows.push(SidebarRow {
                id: format!("meta::{}", pane.pane_id),
                kind: SidebarRowKind::Detail,
                depth: 2,
                label: format!(
                    "{} · {}",
                    format!("{}/{}", pane.category, pane.repo),
                    meta_label(pane, now)
                ),
                chat_count: 0,
                rollup: pane.rollup,
                badge_state: Some(pane.badge_state),
                expanded: true,
                pane_id: Some(pane.pane_id.clone()),
                git: None,
                meta: None,
            });
        }
    }
    rows
}
```

(chat_label ではなく `{agent} · {repo}` を使う理由: 40列で右端の perm/wait と両立させるため。prompt は選択時の meta 行と preview で確認する。§8 の設計判断どおり。)

FLEET 側の `group_meta` は手順 2) の事前計算値を使うよう `repo_rows_from_keyed_map` / `category_rows` に `metas: &BTreeMap<(String, String), RowMeta>` を渡す(triage drain 後の panes からは blocked が消えているため、その場で数えると 0 になってしまう)。

- [ ] **Step 4: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過。既存の runtime テスト(`client_filter_key_rebuilds_rows_to_attention_only` 等)は AttentionOnly の意味変更の影響を受ける — running は残る(従来どおり)、permission/error はテストで ctx.triage が空なら FLEET に残留し AttentionOnly から漏れる。**runtime 側で triage を渡すのは Task 3** なので、この Task の時点では既存 runtime テストが壊れないこと(ctx.triage 空 = 全 pane FLEET、AttentionOnly の変更で Error/Permission/Waiting が漏れる)に注意。壊れる場合は AttentionOnly 判定を「`pane.attention || rollup == Running || (triage が空のときは従来判定)`」にせず、**このタイミングで期待値を先に更新する**(Task 3 完了後の最終挙動が正)。

- [ ] **Step 5: コミット**

```bash
rtk git add -A
rtk git commit -m "blocked pane を TRIAGE ゾーンへ分離して構築する"
```

---

## Task 3: runtime のメンバーシップ管理と退出デバウンス

**Files:**
- Modify: `src/daemon/runtime.rs`(RuntimeState にフィールド追加、`update_triage`、`rebuild_snapshot`)

- [ ] **Step 1: 失敗するテストを書く**

`src/daemon/runtime.rs` tests:

```rust
#[test]
fn blocked_pane_enters_triage_immediately() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let mut blocked = agent_pane("main", "%1", "waiting");
    blocked.wait_reason = "permission_prompt".to_string();
    state.apply_event(DaemonEvent::PanesUpdated(vec![blocked]));

    let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
    assert_eq!(rows[0].id, "zone::triage");
}

#[test]
fn pane_leaves_triage_after_two_calm_polls() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let mut blocked = agent_pane("main", "%1", "waiting");
    blocked.wait_reason = "permission_prompt".to_string();
    state.apply_event(DaemonEvent::PanesUpdated(vec![blocked]));

    let calm = || agent_pane("main", "%1", "running");
    // 1回目の非 blocked: まだ TRIAGE に残る(デバウンス)
    state.apply_event(DaemonEvent::PanesUpdated(vec![calm()]));
    let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
    assert_eq!(rows[0].id, "zone::triage");
    // 2回目: TRIAGE から退出
    state.apply_event(DaemonEvent::PanesUpdated(vec![calm()]));
    let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
    assert!(rows.iter().all(|row| row.id != "zone::triage"));
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib daemon::runtime::tests::blocked_pane_enters_triage_immediately`
Expected: FAIL

- [ ] **Step 3: 実装**

`RuntimeState` にフィールド追加:

```rust
    triage: std::collections::BTreeSet<String>,
    calm_streak: BTreeMap<String, u8>,
```

(`new` で空初期化。)

メソッド追加と配線:

```rust
    const TRIAGE_LEAVE_POLLS: u8 = 2;

    fn update_triage(&mut self) {
        use crate::daemon::session_badge::{BadgeState, badge_state};
        let mut next_triage = std::collections::BTreeSet::new();
        let mut next_streak = BTreeMap::new();
        for pane in self.panes.iter().filter(|pane| is_live_agent_pane(pane)) {
            let level = crate::sidebar::tree::rollup_for_pane(pane);
            let unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
            let blocked = badge_state(level, unread) == BadgeState::Blocked;
            if blocked {
                next_triage.insert(pane.pane_id.clone());
                next_streak.insert(pane.pane_id.clone(), 0);
            } else if self.triage.contains(&pane.pane_id) {
                let streak = self.calm_streak.get(&pane.pane_id).copied().unwrap_or(0) + 1;
                if streak < Self::TRIAGE_LEAVE_POLLS {
                    next_triage.insert(pane.pane_id.clone());
                    next_streak.insert(pane.pane_id.clone(), streak);
                }
            }
        }
        self.triage = next_triage;
        self.calm_streak = next_streak;
    }
```

`apply_event` の `PanesUpdated` 分岐で `self.update_unread();` の直後に `self.update_triage();` を呼ぶ。

`rebuild_snapshot` の `RowBuildContext` に `triage: self.triage.clone()` を渡す。

fingerprint は rows 由来なので追加変更不要(TRIAGE 出入りで rows が変わる)。

- [ ] **Step 4: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過。Task 2 Step 4 で先行更新した AttentionOnly 系の期待値がここで最終挙動と一致することを確認

- [ ] **Step 5: コミット**

```bash
rtk git add -A
rtk git commit -m "TRIAGE メンバーシップを runtime で管理し退出をデバウンスする"
```

---

## Task 4: smoke・ドキュメント・品質ゲート

- [ ] **Step 1: 品質ゲート**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`

- [ ] **Step 2: smoke**

scratch tmux で確認(daemon 再起動込み):
- permission 待ちを発生させる → 最上部に `▍TRIAGE 1` + `▲ codex · repo  perm` が出る
- その間、FLEET の repo 行右端に `▲1` が残る
- `n` で TRIAGE 行へ飛び、Enter で jump、承認 → 約2秒後に TRIAGE が消えて FLEET に戻る
- filter を attention にしても TRIAGE 表示が変わらない
- ちらつき確認: 状態が短時間で往復しても TRIAGE 行が明滅しない

結果を `docs/e2e-smoke.md` に追記。

- [ ] **Step 3: docs 更新とコミット**

`docs/sidebar-ui-proposals.md` §9.2 Phase 3 にチェック。

```bash
rtk git add docs/
rtk git commit -m "Plan 15 の smoke 結果と docs を更新する"
```

## スコープ外

- TRIAGE 行の pin・フル展開 → Plan 16
- スコープセレクタ(category → repo の2段絞り込み)→ Plan 17 のフィルタバー多値化と合わせて再検討

## 実装ノート

- TUI の `capture-pane -a` は Plan 13 と同様に alt-screen が空になるため、Plan 15 の smoke は scratch tmux 上で新 daemon を起動し直し、NDJSON subscribe snapshot の `SidebarFrame.rows` を検証した。
- `SidebarRowKind` / `BadgeState` は JSON wire 上では `Zone` / `Blocked` の PascalCase で出るため、smoke の snapshot assertion は wire 表現に合わせた。
- Plan 18 Task 3・4 で、TRIAGE 保持中の pane を repo/category の `▲N` に含め続け、TRIAGE 選択時に出所 detail 行を表示するよう修正した。
- ゾーン跨ぎ選択移動は Plan 18 Task 1 の `selection_follows_pane_across_triage_and_fleet` で回帰テストを追加した。
