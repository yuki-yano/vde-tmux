# Plan 14: sidebar fisheye 第一段と往復支援(UI 再設計 Phase 2)

> **実装者向け:** `docs/sidebar-ui-proposals.md` §9.2 Phase 2 の実装。**Plan 13 完了が前提**(RowMeta・新グリフ・右端カラムが存在する状態から始める)。Task 順に実施し、各 Task 末尾でテストを通してからコミットする。

**Goal:** 選択した Chat 行の直下に inline meta 1行を自動表示し、`n`/`N` での attention 巡回・jump 時の unread 即時既読化・`vt sidebar focus` による帰還(jump & return)を実現する。

**Architecture:** inline meta は daemon 側(`tree.rs`)で通常の `SidebarRow`(kind: Detail、id: `meta::<pane>`)として挿入する。Detail は `row_refs` から除外されるため選択移動に干渉せず、クリックの行対応(1:1)も維持される。`n`/`N` は `SidebarRowRef` に badge 情報がないため runtime 側で `self.rows` を直接走査する。帰還は sidebar TUI がフォーカス外でキーを受けられないため tmux バインド + 新 CLI で実現する。

**Tech Stack:** Plan 13 と同じ(新規依存なし)

## DoD

### 機能完了条件

- [x] Chat 行を選択すると直下に `13m · task 2/5 · sub 2` 形式の meta 行(DIM)が自動表示され、選択が離れると消える
- [x] Space(明示展開)によるフル詳細(prompt/status/elapsed/session/subagents/jump)は従来どおり動作し、その場合 meta 行は重複表示されない
- [x] `n` / `N` で blocked(badge ▲)の Chat 行間を循環ジャンプできる。blocked が0件なら何も起きない
- [x] Enter またはダブルクリックで jump した pane の unread(✓ done 表示)が即時に消える
- [x] `vt sidebar focus` が現在 window のサイドバー pane にフォーカスを移す。サイドバーが無い window ではエラーメッセージを出して非0終了しない(Ok を返し stderr に出さない。何もしない)
- [x] README に tmux バインド例(`bind-key` で `vt sidebar focus`)が記載される

### テスト完了条件

- [x] `rtk cargo test` 全通過
- [x] 新規テスト: meta 行の生成/非生成条件、n/N の循環と0件時 no-op、jump 時 unread クリア、focus の select-pane 発行
- [x] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [x] `docs/e2e-smoke.md` に「選択で meta 行が出る」「n で ▲ に飛ぶ」「jump で ✓ が消える」「prefix キーで sidebar に戻れる」を追記し、smoke 実施を記録
- [x] README に focus のバインド例を追加
- [x] `docs/sidebar-ui-proposals.md` §9.2 Phase 2 にチェック

---

## Task 0: 選択 Chat 行直下の inline meta 行

**Files:**
- Modify: `src/sidebar/tree.rs`(`push_chat_row` 286-310行、meta 行生成ヘルパ追加)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/tree.rs` tests に追加(既存テストの pane 構築パターンを流用):

```rust
#[test]
fn selected_chat_row_gets_inline_meta_row() {
    // pane: prompt="fix bug", started_at=(now-815), tasks="2/5",
    //       subagents="sub1:Explore|ab12:general-purpose", status="running"
    // state: selection=Some("chat::%1")、chat::%1 は未展開(デフォルト)
    let rows = /* build_rows_at(...) ViewMode::Flat, now 固定 */;
    let meta = rows
        .iter()
        .find(|row| row.id == "meta::%1")
        .expect("meta row");
    assert_eq!(meta.kind, SidebarRowKind::Detail);
    assert_eq!(meta.label, "13m · task 2/5 · sub 2");
    assert_eq!(meta.pane_id.as_deref(), Some("%1"));
}

#[test]
fn unselected_or_expanded_chat_rows_have_no_meta_row() {
    // (a) selection=None → meta::%1 が無い
    // (b) selection=Some("chat::%1") かつ chat::%1 を toggle_expanded 済み
    //     → フル Detail 行が出て meta::%1 は無い
}

#[test]
fn meta_row_falls_back_to_session_and_pane() {
    // prompt/tasks/subagents/started_at がすべて空の pane を選択
    // → label が "main / %1"(session / pane_id)になる
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::tree::tests::selected_chat_row_gets_inline_meta_row`
Expected: FAIL(meta 行が存在しない)

- [ ] **Step 3: 実装**

`src/sidebar/tree.rs` にヘルパを追加し、`push_chat_row`(286-310行)の末尾を変更:

```rust
fn meta_label(pane: &AgentPane, now: i64) -> String {
    let mut parts = Vec::new();
    if let Ok(started_at) = pane.started_at.parse::<i64>() {
        let elapsed = (now - started_at).max(0);
        if elapsed < 60 {
            parts.push(format!("{elapsed}s"));
        } else {
            parts.push(format!("{}m", elapsed / 60));
        }
    }
    if let Some((done, total)) = parse_tasks(&pane.tasks) {
        parts.push(format!("task {done}/{total}"));
    }
    let subagents = decode_subagents(&pane.subagents).len();
    if subagents > 0 {
        parts.push(format!("sub {subagents}"));
    }
    if let Some(wait_reason) = non_empty(&pane.wait_reason) {
        parts.push(wait_reason.to_string());
    }
    if parts.is_empty() {
        format!("{} / {}", pane.session, pane.pane_id)
    } else {
        parts.join(" · ")
    }
}
```

`push_chat_row` の `if expanded { ... }` を次に置き換える:

```rust
    if expanded {
        push_chat_detail_rows(pane, depth + 1, now, rows);
    } else if state.selection.as_deref() == Some(id.as_str()) {
        rows.push(SidebarRow {
            id: format!("meta::{}", pane.pane_id),
            kind: SidebarRowKind::Detail,
            depth: depth + 1,
            label: meta_label(pane, now),
            chat_count: 0,
            rollup: pane.rollup,
            badge_state: Some(pane.badge_state),
            expanded: true,
            pane_id: Some(pane.pane_id.clone()),
            git: None,
            meta: None,
        });
    }
```

(`id` は push 前に `let id = format!("chat::{}", pane.pane_id);` で束縛済み。`rows.push` で move される前に `state.selection` と比較できるよう、比較を先に行って `let show_meta = ...` に束縛してから push する形にしてもよい。コンパイルエラーになる場合はそちらに直す。)

- [ ] **Step 4: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過。`client_move_selection_skips_detail_rows_and_can_select_jump_row`(runtime.rs:617)等の既存テストは、meta 行が Detail kind であり `row_refs` から除外されるため影響を受けないはず。失敗した場合は選択遷移の期待値を確認する(meta 行はクリック時 PreviewPane として振る舞う。これは仕様)

- [ ] **Step 5: コミット**

```bash
rtk git add -A
rtk git commit -m "選択中の chat 行直下に inline meta 行を表示する"
```

---

## Task 1: n / N での attention 巡回

**Files:**
- Modify: `src/sidebar/input.rs`(`SidebarInputAction` 4-19行、`parse_key` 28-50行)
- Modify: `src/daemon/runtime.rs`(`apply_key` 288-344行)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/input.rs` tests:

```rust
#[test]
fn parse_key_maps_attention_navigation() {
    assert_eq!(parse_key("n"), Some(SidebarInputAction::FocusNextAttention));
    assert_eq!(
        parse_key("N"),
        Some(SidebarInputAction::FocusPreviousAttention)
    );
}
```

`src/daemon/runtime.rs` tests(既存 `agent_pane` ヘルパを使用。blocked は `wait_reason = "permission_prompt"` の waiting pane で作る):

```rust
#[test]
fn attention_navigation_cycles_blocked_chat_rows() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let mut blocked_a = agent_pane("main", "%1", "waiting");
    blocked_a.wait_reason = "permission_prompt".to_string();
    let mut blocked_b = agent_pane("main", "%3", "waiting");
    blocked_b.wait_reason = "permission_prompt".to_string();
    state.apply_event(DaemonEvent::PanesUpdated(vec![
        blocked_a,
        agent_pane("main", "%2", "running"),
        blocked_b,
    ]));

    let key = |state: &mut RuntimeState, key: &str| {
        state.apply_event(DaemonEvent::Client {
            client_id: ClientId(1),
            event: SidebarClientEvent::Key {
                key: key.to_string(),
            },
        });
    };

    key(&mut state, "n");
    assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
    key(&mut state, "n");
    assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%3"));
    key(&mut state, "n"); // 循環して先頭へ
    assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%1"));
    key(&mut state, "N"); // 逆方向
    assert_eq!(state.ui_state.selection.as_deref(), Some("chat::%3"));
}

#[test]
fn attention_navigation_is_noop_without_blocked_rows() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    state.apply_event(DaemonEvent::Client {
        client_id: ClientId(1),
        event: SidebarClientEvent::Key {
            key: "n".to_string(),
        },
    });
    assert_eq!(state.ui_state.selection, None);
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::input::tests::parse_key_maps_attention_navigation`
Expected: コンパイルエラー(variant 未定義)

- [ ] **Step 3: 実装**

`src/sidebar/input.rs`: `SidebarInputAction` に `FocusNextAttention` / `FocusPreviousAttention` を追加し、`parse_key` の match に追加:

```rust
        "n" => Some(SidebarInputAction::FocusNextAttention),
        "N" => Some(SidebarInputAction::FocusPreviousAttention),
```

`src/daemon/runtime.rs` の `apply_key` の match に分岐を追加:

```rust
            SidebarInputAction::FocusNextAttention => self.focus_attention(true),
            SidebarInputAction::FocusPreviousAttention => self.focus_attention(false),
```

`RuntimeState` にメソッド追加:

```rust
    fn focus_attention(&mut self, forward: bool) -> bool {
        use crate::daemon::session_badge::BadgeState;
        let blocked: Vec<&str> = self
            .rows
            .iter()
            .filter(|row| {
                row.kind == SidebarRowKind::Chat
                    && row.badge_state == Some(BadgeState::Blocked)
            })
            .map(|row| row.id.as_str())
            .collect();
        if blocked.is_empty() {
            return false;
        }
        let current = self
            .ui_state
            .selection
            .as_deref()
            .and_then(|id| blocked.iter().position(|blocked_id| *blocked_id == id));
        let next = match (current, forward) {
            (None, true) => 0,
            (None, false) => blocked.len() - 1,
            (Some(index), true) => (index + 1) % blocked.len(),
            (Some(index), false) => (index + blocked.len() - 1) % blocked.len(),
        };
        let next_id = blocked[next].to_string();
        if self.ui_state.selection.as_deref() == Some(next_id.as_str()) {
            return false;
        }
        self.ui_state.selection = Some(next_id);
        self.ui_state.version += 1;
        true
    }
```

- [ ] **Step 4: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過(blocked 行が折りたたまれた repo 内にある場合は rows に現れないため巡回対象外。この制限は Plan 15 の TRIAGE 常設ゾーンで解消される — 既知の制限としてテストにコメントを残す)

- [ ] **Step 5: コミット**

```bash
rtk git add -A
rtk git commit -m "n/N で blocked な chat 行を循環選択できるようにする"
```

---

## Task 2: jump 時の unread 即時クリア

**Files:**
- Modify: `src/daemon/runtime.rs`(`apply_client_event` 275-286行、`apply_key` の Activate 分岐 319-336行)

- [ ] **Step 1: 失敗するテストを書く**

`src/daemon/runtime.rs` tests:

```rust
#[test]
fn jump_clears_unread_immediately() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    // running → idle で unread(done)を作る
    let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "idle",
    )]));

    let effects = state.apply_event(DaemonEvent::Client {
        client_id: ClientId(1),
        event: SidebarClientEvent::JumpPane {
            pane: "%1".to_string(),
        },
    });

    assert!(effects.contains(&RuntimeEffect::JumpPane("%1".to_string())));
    let rows = &state.snapshot().unwrap().sidebar.as_ref().unwrap().rows;
    let chat = rows.iter().find(|row| row.id == "chat::%1").unwrap();
    // unread が消えたので Done(✓)ではなく Idle(○)
    assert_eq!(
        chat.badge_state,
        Some(crate::daemon::session_badge::BadgeState::Idle)
    );
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib daemon::runtime::tests::jump_clears_unread_immediately`
Expected: FAIL(badge_state が Done のまま)

- [ ] **Step 3: 実装**

`apply_client_event` の `JumpPane` 分岐(278-284行)に unread クリアを追加:

```rust
            SidebarClientEvent::JumpPane { pane } => {
                self.unread.insert(pane.clone(), false);
                self.ui_state.selection = Some(format!("chat::{pane}"));
                self.mark_state_dirty(Instant::now());
                self.rebuild_snapshot();
                self.broadcast_if_needed();
                vec![RuntimeEffect::JumpPane(pane)]
            }
```

`apply_key` の Activate → JumpPane 分岐(321-323行)も同様に:

```rust
                    Some(SidebarCommand::JumpPane(pane_id)) => {
                        self.unread.insert(pane_id.clone(), false);
                        self.rebuild_snapshot();
                        self.broadcast_if_needed();
                        return vec![RuntimeEffect::JumpPane(pane_id)];
                    }
```

注意: `update_unread`(395-419行)は次ポーリングで `self.unread` を再構築するが、jump 直後は `pane.window_active && pane.session_attached` が真になるため false のまま維持される。ポーリングより先に snapshot を見た場合の一瞬の ✓ 復活を防ぐのが本 Task の目的。

- [ ] **Step 4: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過

- [ ] **Step 5: コミット**

```bash
rtk git add -A
rtk git commit -m "jump 時に unread を即時クリアする"
```

---

## Task 3: vt sidebar focus と jump & return

**Files:**
- Modify: `src/sidebar/layout.rs`(`focus` 追加。`find_sidebar_pane` は既存)
- Modify: `src/cli/sidebar.rs`(`Focus` サブコマンド)
- Modify: `README.md`(バインド例)

- [ ] **Step 1: 失敗するテストを書く**

`src/cli/tests/sidebar.rs` に追加(既存の dispatch 系テストのモック runner パターンを流用):

```rust
#[test]
fn dispatch_sidebar_focus_selects_sidebar_pane() {
    // モック runner:
    //   list-panes(SIDEBAR_PANE_FORMAT)に "%9\t1\t40" を返す
    // vt sidebar focus 実行後、runner の呼び出し履歴に
    //   ["select-pane", "-t", "%9"] が含まれることを検証
}

#[test]
fn dispatch_sidebar_focus_without_sidebar_is_noop() {
    // list-panes が sidebar なし("%1\t\t80" のみ)を返す場合、
    // select-pane が呼ばれず Ok(None) が返ることを検証
}
```

(モック runner の組み立ては同ファイルの `dispatch_sidebar_jump_forwards_to_daemon_when_socket_exists`(319行)等と同じ流儀にする。)

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib cli::tests::sidebar`
Expected: コンパイルエラー(`Focus` variant 未定義)

- [ ] **Step 3: 実装 — layout.rs**

`src/sidebar/layout.rs` に追加(`find_sidebar_pane` / `resolve_window_target` 相当は既存のものを使う):

```rust
pub fn focus(runner: &dyn TmuxRunner, target: &str) -> Result<()> {
    let Some(sidebar) = find_sidebar_pane(runner, target)? else {
        return Ok(());
    };
    runner.run(&["select-pane", "-t", &sidebar.pane_id])?;
    Ok(())
}
```

- [ ] **Step 4: 実装 — cli/sidebar.rs**

`SidebarCommand` enum に追加:

```rust
    Focus {
        #[arg(long)]
        window: Option<String>,
    },
```

`run_sidebar_command_with_ensure` の match に追加:

```rust
        SidebarCommand::Focus { window } => {
            let target = resolve_window_target(runner, window)?;
            crate::sidebar::layout::focus(runner, &target)?;
            Ok(None)
        }
```

- [ ] **Step 5: README にバインド例を追記**

README のサイドバー節に追加:

```markdown
### jump & return

サイドバーから Enter で agent pane に jump した後、tmux バインドで sidebar に戻れます:

​```tmux
bind-key b run-shell "vt sidebar focus"
​```
```

- [ ] **Step 6: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過

- [ ] **Step 7: コミット**

```bash
rtk git add -A
rtk git commit -m "vt sidebar focus を追加して jump & return を可能にする"
```

---

## Task 4: smoke・ドキュメント・品質ゲート

- [ ] **Step 1: 品質ゲート**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`
Expected: すべて通過

- [ ] **Step 2: smoke**

scratch tmux で確認(daemon 再起動込み):
- Chat 行を j/k で選択 → 直下に meta 行が出る/離れると消える
- Space でフル詳細を開くと meta 行は出ない
- permission 待ち pane を2つ作り `n`/`N` で循環
- done(✓)の pane に Enter で jump → サイドバー表示が即 ○ になる
- `bind-key b run-shell "vt sidebar focus"` を設定し、jump → `prefix b` で戻る

結果を `docs/e2e-smoke.md` に追記。

- [ ] **Step 3: docs 更新とコミット**

`docs/sidebar-ui-proposals.md` §9.2 Phase 2 にチェック。

```bash
rtk git add docs/
rtk git commit -m "Plan 14 の smoke 結果と docs を更新する"
```

## スコープ外

- blocked 行が折りたたみ内に隠れて n/N の対象にならない問題 → Plan 15(TRIAGE 常設)で構造的に解消
- meta 行の複数行化(フル展開の選択連動)→ Plan 16

## 実装ノート

- DoD 1・2(選択時 inline meta)は Plan 16 の3段階行高でフル展開仕様に上書きされた。現仕様では meta 1行は pin 時のみ表示する。
