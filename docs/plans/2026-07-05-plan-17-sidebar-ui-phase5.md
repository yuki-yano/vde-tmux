# Plan 17: LIVE ペインとフィルタバー多値化(UI 再設計 Phase 5)

> **実装者向け:** `docs/sidebar-ui-proposals.md` §9.2 Phase 5 の実装。**Plan 13〜16 完了が前提**。Task 順に実施する。

**Goal:** 選択 agent の画面末尾を実況する LIVE ペイン(`e` でイベントログに切替)、ヘッダーの icon+count フィルタバー化(多値フィルタ)、状態変化フラッシュ、デスクトップ通知(opt-in)を追加する。

**Architecture:** LIVE は client 側の transient 機能(`capture-pane` を低頻度実行、daemon snapshot には混ぜない)。イベントログと状態変化フラッシュは daemon 側で badge 遷移を記録して snapshot に載せる(`DaemonSnapshot.events`、`RowMeta.flash`)。フィルタバーは `StatusFilter` の enum 拡張 + ヘッダーを counts 付きセグメント列に置き換える。通知は daemon の遷移検知から `RuntimeEffect` 経由で外部コマンドを spawn する。

**Tech Stack:** Plan 13〜16 と同じ(新規依存なし)

## DoD

### 機能完了条件

- [ ] サイドバー下部(フッターの上)に `LIVE · {pane_id}` 見出し + 選択 agent pane の末尾3行が表示され、選択変更・2秒間隔で更新される。`sidebar.live.enabled: false` で無効化できる
- [ ] `e` で LIVE がイベントログ(`2m前 codex ● → ▲` 形式の遷移フィード)に切り替わる(トグル)
- [ ] ヘッダーが `" {mode} · ≡6 ▲2 ●1 ✓1 ○2"` になり、各状態セグメントのクリックでそのフィルタに直接切り替わる。`Tab` は all → blocked → working → done → idle → all の巡回になる
- [ ] フィルタは FLEET にのみ作用し、TRIAGE は常に貫通(Plan 15 の性質を維持)
- [ ] badge が変化した行が2ポーリング(約2秒)の間 REVERSED でフラッシュする
- [ ] `notify.enabled: true` かつ `notify.command` 設定時、blocked / error への遷移で外部コマンドが1回実行される(既定は無効)
- [ ] 経過時間表示が毎秒更新される(Plan 13 の fingerprint 変更により daemon push で実現済みであることの確認)

### テスト完了条件

- [ ] `rtk cargo test` 全通過
- [ ] 新規テスト: StatusFilter 拡張のフィルタ挙動と巡回、ヘッダー counts とセグメント hit-test、遷移イベントの記録と上限、flash の付与と消滅、通知 effect の発火条件、LIVE 領域のレイアウト
- [ ] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [ ] `docs/e2e-smoke.md` に LIVE・イベントログ・フィルタバー・フラッシュの確認手順を追記し、smoke 実施を記録
- [ ] README に `sidebar.live` / `notify` 設定例を追記
- [ ] `docs/sidebar-ui-proposals.md` §9.2 Phase 5 にチェック

---

## Task 0: StatusFilter の多値化

**Files:**
- Modify: `src/sidebar/state.rs:35-41, 238-245`
- Modify: `src/sidebar/tree.rs`(`pane_matches_filter` 429-440行)
- Modify: `src/sidebar/input.rs`(SetFilter キー)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/state.rs` tests:

```rust
#[test]
fn filter_cycles_through_all_states() {
    let mut filter = StatusFilter::All;
    let expected = [
        StatusFilter::AttentionOnly,
        StatusFilter::WorkingOnly,
        StatusFilter::DoneOnly,
        StatusFilter::IdleOnly,
        StatusFilter::All,
    ];
    for want in expected {
        filter = filter.next();
        assert_eq!(filter, want);
    }
}

#[test]
fn old_state_json_filter_value_still_loads() {
    let state: SidebarState =
        serde_json::from_str(r#"{"version":1,"filter":"attention_only"}"#).unwrap();
    assert_eq!(state.filter, StatusFilter::AttentionOnly);
}
```

`src/sidebar/tree.rs` tests:

```rust
#[test]
fn working_done_idle_filters_partition_fleet_panes() {
    // running / idle+unread(done) / idle(既読) の3体で
    // WorkingOnly → running のみ、DoneOnly → done のみ、IdleOnly → idle のみ
    // (BadgeState ベースで判定する)
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::state::tests::filter_cycles_through_all_states`
Expected: コンパイルエラー

- [ ] **Step 3: 実装**

`src/sidebar/state.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusFilter {
    #[default]
    All,
    AttentionOnly,
    WorkingOnly,
    DoneOnly,
    IdleOnly,
}

impl StatusFilter {
    pub fn next(self) -> Self {
        match self {
            StatusFilter::All => StatusFilter::AttentionOnly,
            StatusFilter::AttentionOnly => StatusFilter::WorkingOnly,
            StatusFilter::WorkingOnly => StatusFilter::DoneOnly,
            StatusFilter::DoneOnly => StatusFilter::IdleOnly,
            StatusFilter::IdleOnly => StatusFilter::All,
        }
    }
}
```

`src/sidebar/tree.rs` の `pane_matches_filter` を BadgeState ベースに書き換え(FLEET 側にのみ適用される前提。Plan 15 で blocked は TRIAGE へ移動済みだが、デバウンス中の残留に備えて Blocked も判定に含める):

```rust
fn pane_matches_filter(pane: &AgentPane, filter: StatusFilter) -> bool {
    match filter {
        StatusFilter::All => true,
        StatusFilter::AttentionOnly => {
            pane.attention || pane.badge_state == BadgeState::Blocked
                || pane.badge_state == BadgeState::Working
        }
        StatusFilter::WorkingOnly => pane.badge_state == BadgeState::Working,
        StatusFilter::DoneOnly => pane.badge_state == BadgeState::Done,
        StatusFilter::IdleOnly => pane.badge_state == BadgeState::Idle,
    }
}
```

`src/sidebar/input.rs` の `parse_key` に直接指定キーを追加(`all`/`attn` は既存):

```rust
        "working" => Some(SidebarInputAction::SetFilter(StatusFilter::WorkingOnly)),
        "done" => Some(SidebarInputAction::SetFilter(StatusFilter::DoneOnly)),
        "idle" => Some(SidebarInputAction::SetFilter(StatusFilter::IdleOnly)),
```

`filter_label`(render.rs)に新 variant のラベル(`working`/`done`/`idle`)を追加。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "StatusFilter を badge 状態ベースの多値に拡張する"
```

---

## Task 1: フィルタバー(icon + count ヘッダー)

**Files:**
- Modify: `src/sidebar/render.rs`(build_header_layout 系、HeaderAction)
- Modify: `src/sidebar/tui.rs`(counts の受け渡し、クリック)

- [ ] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests:

```rust
#[test]
fn header_shows_badge_counts_as_filter_segments() {
    let counts = BadgeCounts {
        total: 6,
        blocked: 2,
        working: 1,
        done: 1,
        idle: 2,
    };
    let state = SidebarState::default();
    let header = build_header_layout_with_counts(
        &state,
        60,
        &SidebarRenderTheme::default(),
        &counts,
    );
    assert_eq!(header.lines[0].text, " repo · ≡6 ▲2 ●1 ✓1 ○2");
    // "≡6" クリック → All、"▲2" → AttentionOnly、"●1" → WorkingOnly ...
    let column_of = |needle: &str| {
        header.lines[0].text.find(needle).unwrap() as u16
    };
    assert_eq!(
        header_hit_test(&header, 0, column_of("≡")),
        Some(HeaderAction::SetFilter(StatusFilter::All))
    );
    assert_eq!(
        header_hit_test(&header, 0, column_of("▲")),
        Some(HeaderAction::SetFilter(StatusFilter::AttentionOnly))
    );
    assert_eq!(
        header_hit_test(&header, 0, column_of("○")),
        Some(HeaderAction::SetFilter(StatusFilter::IdleOnly))
    );
}
```

(注: `text.find` は byte index を返すため ASCII 以外を含む本テキストでは cell column と一致しない。テストでは `text.chars().position(...)` ベースのヘルパを書いて cell column に換算する。`≡▲●✓○·` はすべて幅1なので char index == cell column が成り立つ。)

- [ ] **Step 2: テストが失敗することを確認**

Run: 上記テスト
Expected: コンパイルエラー(BadgeCounts / build_header_layout_with_counts / HeaderAction::SetFilter 未定義)

- [ ] **Step 3: 実装**

`src/sidebar/render.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BadgeCounts {
    pub total: usize,
    pub blocked: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
}

impl BadgeCounts {
    pub fn from_rows(rows: &[SidebarRow]) -> Self {
        use crate::daemon::session_badge::BadgeState;
        let mut counts = Self::default();
        for row in rows.iter().filter(|row| row.kind == SidebarRowKind::Chat) {
            counts.total += 1;
            match row.badge_state {
                Some(BadgeState::Blocked) => counts.blocked += 1,
                Some(BadgeState::Working) => counts.working += 1,
                Some(BadgeState::Done) => counts.done += 1,
                Some(BadgeState::Idle) | None => counts.idle += 1,
            }
        }
        counts
    }
}
```

`HeaderAction` を拡張:

```rust
pub enum HeaderAction {
    CycleViewMode,
    ToggleFilter,
    SetFilter(StatusFilter),
}
```

`build_header_layout_with_counts(state, width, theme, counts) -> HeaderLayout` を新設し、`" {mode} · ≡{total} ▲{blocked} ●{working} ✓{done} ○{idle}"` を構築する。mode セグメントは `CycleViewMode`、各カウントセグメントは `SetFilter(...)`。現在アクティブなフィルタのセグメントは `header_segment_style`(既存の active 色)を適用し、他は各 badge 色。`render_header_lines` はセグメント単位のスタイル付けを既に行っているため、`HeaderSegment` に `style: Option<Style>` を追加してセグメント別スタイルを流せるようにする。既存の `build_header_layout_with_theme` は counts ゼロで委譲する互換 wrapper として残す(既存テスト用)。

`src/sidebar/tui.rs`:
- draw: `BadgeCounts::from_rows(&sidebar.rows)` を計算して `build_header_layout_with_counts` を使用
- `handle_left_click` の header 分岐に `Some(HeaderAction::SetFilter(filter))` を追加し、filter に応じたキー文字列(`all`/`attn`/`working`/`done`/`idle`)を `send_sidebar_key` で送る

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`(旧ヘッダーテキストの既存テストは counts ゼロ wrapper 経由で `" repo · ≡0 ▲0 ●0 ✓0 ○0"` になるため期待値を更新する)

```bash
rtk git add -A
rtk git commit -m "header を icon+count のフィルタバーにする"
```

---

## Task 2: 遷移イベントの記録とフラッシュ

**Files:**
- Modify: `src/daemon/mod.rs`(DaemonSnapshot.events)
- Modify: `src/daemon/runtime.rs`(遷移記録)
- Modify: `src/sidebar/tree.rs`(RowMeta.flash)/ `src/sidebar/render.rs`(REVERSED)

- [ ] **Step 1: 失敗するテストを書く**

`src/daemon/runtime.rs` tests:

```rust
#[test]
fn badge_transitions_are_recorded_as_events() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "idle",
    )]));

    let events = &state.snapshot().unwrap().events;
    assert_eq!(events.len(), 2); // 出現(→Working) と Working→Done
    let last = events.last().unwrap();
    assert_eq!(last.pane_id, "%1");
    assert_eq!(last.to, crate::daemon::session_badge::BadgeState::Done);
}

#[test]
fn events_are_capped_at_20() {
    // 21回遷移させて len == 20、最古が落ちることを検証
}

#[test]
fn changed_rows_flash_for_two_polls() {
    // 遷移直後の rows で chat の meta.flash == Some(true)、
    // さらに2回同状態で PanesUpdated すると flash == Some(false)
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib daemon::runtime::tests::badge_transitions_are_recorded_as_events`
Expected: コンパイルエラー

- [ ] **Step 3: 実装**

`src/daemon/mod.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionEvent {
    pub pane_id: String,
    pub agent: String,
    pub from: Option<crate::daemon::session_badge::BadgeState>,
    pub to: crate::daemon::session_badge::BadgeState,
    pub at_epoch: i64,
}
```

`DaemonSnapshot` に `#[serde(default)] pub events: Vec<TransitionEvent>` を追加(`build_snapshot_with_sidebar` の呼び出し側で埋める。シグネチャ変更が波及する場合は snapshot 構築後に `snapshot.events = ...` を代入する形でよい)。

`src/daemon/runtime.rs` の `RuntimeState`:

```rust
    prev_badges: BTreeMap<String, crate::daemon::session_badge::BadgeState>,
    events: std::collections::VecDeque<TransitionEvent>,
    flash: BTreeMap<String, u8>, // 残りポーリング数
```

`PanesUpdated` 処理(update_unread / update_triage の後)に `update_transitions()` を追加:

```rust
    const FLASH_POLLS: u8 = 2;
    const EVENT_CAP: usize = 20;

    fn update_transitions(&mut self) {
        use crate::daemon::session_badge::{badge_state};
        let mut next_badges = BTreeMap::new();
        for pane in self.panes.iter().filter(|pane| is_live_agent_pane(pane)) {
            let level = crate::sidebar::tree::rollup_for_pane(pane);
            let unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
            let badge = badge_state(level, unread);
            let prev = self.prev_badges.get(&pane.pane_id).copied();
            if prev != Some(badge) {
                self.events.push_back(TransitionEvent {
                    pane_id: pane.pane_id.clone(),
                    agent: pane.agent.clone(),
                    from: prev,
                    to: badge,
                    at_epoch: crate::sidebar::tree::now_epoch_secs(),
                });
                while self.events.len() > Self::EVENT_CAP {
                    self.events.pop_front();
                }
                self.flash.insert(pane.pane_id.clone(), Self::FLASH_POLLS);
            }
            next_badges.insert(pane.pane_id.clone(), badge);
        }
        self.prev_badges = next_badges;
        self.flash.retain(|_, remaining| {
            *remaining = remaining.saturating_sub(1);
            *remaining > 0
        });
    }
```

(flash のデクリメントは「遷移したポーリングで挿入 → 次とその次のポーリングで表示 → 消滅」となるよう、挿入と retain の順序に注意。テストが正: 遷移直後 flash=true、2回の安定後 false。)

`RowBuildContext` に `flash: BTreeSet<String>` を追加し、`chat_meta` で `flash: Some(ctx.flash.contains(&pane.pane_id))` を `RowMeta` に載せる(`RowMeta` に `pub flash: Option<bool>` を追加)。

`render_row_line`: badge span の style に `if flash { .add_modifier(Modifier::REVERSED) }` を適用。

イベントログの描画は Task 3(LIVE)で行う。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "badge 遷移イベントの記録と行フラッシュを追加する"
```

---

## Task 3: LIVE ペインとイベントログ切替

**Files:**
- Modify: `src/config/mod.rs`(SidebarLiveConfig)
- Modify: `src/sidebar/tui.rs`(capture・レイアウト・`e` トグル)

- [ ] **Step 1: 失敗するテストを書く**

`src/config/mod.rs` tests(既存 config テストの流儀):

```rust
#[test]
fn sidebar_live_config_defaults_and_overrides() {
    let config = Config::default();
    assert!(config.sidebar.live.enabled);
    assert_eq!(config.sidebar.live.lines, 3);
    assert_eq!(config.sidebar.live.interval_ms, 2000);
}
```

`src/sidebar/tui.rs` tests:

```rust
#[test]
fn compute_areas_reserves_live_rows_when_enabled() {
    // 幅40 高さ24, live_lines=3 → header 1 / rows 18 / live 4(見出し+3) / footer 1
    // 高さ 14 未満では live_rows == 0(rows 領域を優先)
}

#[test]
fn live_tail_keeps_last_nonempty_lines() {
    // extract_tail("a\nb\n\nc\n\n\n", 3) == ["a", "b", "c"] 相当の
    // 末尾非空行抽出ヘルパを検証
}
```

- [ ] **Step 2: テストが失敗することを確認 → 実装**

`src/config/mod.rs` の `SidebarConfig` に `pub live: SidebarLiveConfig` を追加:

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SidebarLiveConfig {
    pub enabled: bool,
    pub lines: u16,
    pub interval_ms: u64,
}

impl Default for SidebarLiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lines: 3,
            interval_ms: 2000,
        }
    }
}
```

`src/sidebar/tui.rs`:
- `compute_areas` に `live_lines: u16` 引数を追加(0 なら確保しない。確保条件: `area.height >= 14 && area.width > 2`。確保幅は `live_lines + 1`(見出し行))
- run loop に状態を追加: `live_buffer: Vec<String>`、`live_mode: LiveMode`(`Tail` / `Events`)、`last_capture: Instant`
- キー処理: `"e"` はローカルで `live_mode` をトグルし daemon へ送らない(`p` と同じ扱いの分岐に追加)
- 描画ループの tick(`crossterm::event::poll` のタイムアウト)を利用し、`interval_ms` 経過かつ選択 pane_id があれば `runner.run(&["capture-pane", "-p", "-t", pane_id])` を実行、`extract_tail(&output, lines)` を `live_buffer` に保存
- draw: live 領域に `LIVE · {pane_id}`(DIM)+ tail 行、または `live_mode == Events` なら `DaemonSnapshot.events` の末尾 N 件を `{Δ}s前 {agent} {from_glyph} → {to_glyph}` 形式(新しい順)で描画

```rust
pub(crate) fn extract_tail(output: &str, lines: u16) -> Vec<String> {
    output
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(lines as usize)
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}
```

クリック境界(`handle_left_click`)の rows 領域判定は `compute_areas` 経由なので live 分は自動で除外される(Plan 13 Task 5 の構造が効く)。

- [ ] **Step 3: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "選択 pane の LIVE 表示とイベントログ切替を追加する"
```

---

## Task 4: デスクトップ通知(opt-in)

**Files:**
- Modify: `src/config/mod.rs`(NotifyConfig)
- Modify: `src/daemon/runtime.rs`(RuntimeEffect::Notify)/ `src/daemon/server.rs`(spawn)

- [ ] **Step 1: 失敗するテストを書く**

`src/daemon/runtime.rs` tests:

```rust
#[test]
fn blocked_transition_emits_notify_effect_when_enabled() {
    let mut config = Config::default();
    config.notify.enabled = true;
    config.notify.command = "true".to_string();
    let mut state = RuntimeState::new(config, SidebarState::default());
    state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    let mut blocked = agent_pane("main", "%1", "waiting");
    blocked.wait_reason = "permission_prompt".to_string();

    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![blocked]));

    assert!(effects.iter().any(|effect| matches!(
        effect,
        RuntimeEffect::Notify { pane_id, .. } if pane_id == "%1"
    )));
}

#[test]
fn notify_is_silent_by_default_and_for_non_blocked_transitions() {
    // 既定 config では blocked 遷移でも Notify effect が出ない。
    // enabled でも Working→Done では出ない(blocked/error のみ)
}
```

- [ ] **Step 2: テストが失敗することを確認 → 実装**

`src/config/mod.rs`(トップレベル Config に `pub notify: NotifyConfig` を追加):

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub command: String,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: String::new(),
        }
    }
}
```

`RuntimeEffect` に追加:

```rust
    Notify {
        pane_id: String,
        agent: String,
        state: String, // "blocked" | "error"
    },
```

`update_transitions`(Task 2)で遷移記録時に、`config.notify.enabled && !command.is_empty()` かつ `to == BadgeState::Blocked` のとき effect を積む(`update_transitions` の戻り値を `Vec<RuntimeEffect>` にして `PanesUpdated` 分岐で `sync_session_badges()` の結果と連結する)。

`src/daemon/server.rs` の `handle_runtime_effects` に分岐追加:

```rust
            RuntimeEffect::Notify { pane_id, agent, state } => {
                let command = config.notify.command.clone();
                let _ = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&command)
                    .env("VDE_NOTIFY_PANE", pane_id)
                    .env("VDE_NOTIFY_AGENT", agent)
                    .env("VDE_NOTIFY_STATE", state)
                    .spawn();
            }
```

(server.rs 側で config が参照できる形は既存 effect 処理の実装に合わせる。README に `terminal-notifier` / `osascript` を使う command 例を記載する。)

- [ ] **Step 3: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "blocked 遷移の opt-in デスクトップ通知を追加する"
```

---

## Task 5: smoke・ドキュメント・品質ゲート

- [ ] **Step 1: 品質ゲート**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`

- [ ] **Step 2: smoke**

scratch tmux で確認(daemon 再起動込み):
- 選択を動かすと LIVE 見出しと末尾3行が追従、約2秒で内容更新
- `e` でイベントログに切替、遷移履歴が新しい順に見える
- ヘッダーの `▲2` をクリック → blocked フィルタ、`≡` で all に復帰、Tab で5値巡回
- permission 発生時に該当行のグリフが約2秒 REVERSED でフラッシュ
- notify を有効化した config で blocked 遷移時に通知コマンドが走る
- running 中の agent の経過時間が毎秒進む

結果を `docs/e2e-smoke.md` に追記。

- [ ] **Step 3: docs 更新とコミット**

README に live / notify の設定例、`docs/sidebar-ui-proposals.md` §9.2 Phase 5 にチェック。

```bash
rtk git add -A
rtk git commit -m "Plan 17 の smoke 結果と docs を更新する"
```

## スコープ外

- スコープセレクタ(category→repo 2段絞り込み)— フィルタバーで密度が上がったヘッダーに同居させる設計を Phase 6 で判断
- LIVE の複数 pane 同時表示・分割 — Phase 6

## 実装ノート

- `RuntimeEffect::Notify.state` は文字列ではなく `BadgeState` のまま保持し、server 側で `VDE_BADGE_STATE` に `Debug` 表記(`Blocked` など)として渡す実装にした。runtime 内の型安全性を保ち、wire format には載せない transient effect のため。
- notify command の環境変数名は実装側で既存 prefix に合わせて `VDE_PANE_ID` / `VDE_AGENT` / `VDE_BADGE_STATE` とした。
- TUI の `capture-pane -a` は Plan 15/16 と同様に alt-screen 内容を安定取得できないため、Plan 17 smoke では daemon subscribe snapshot と scratch pane の `capture-pane -p`、および `sidebar::tui` unit test で LIVE / event / flash / filter の確認を分担した。
