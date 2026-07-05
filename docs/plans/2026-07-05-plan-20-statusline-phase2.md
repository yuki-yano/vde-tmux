# Plan 20: statusline summary への置換(statusline 再設計 Step 2)

> **実装者向け:** `docs/statusline-ui-proposals.md` §7.2 Step 2 の実装。**Plan 19 完了が前提**。Task 順に TDD で進める。

**Goal:** `vt statusline-agent-badge`(`running:2` 形式)を廃止し、状態別カウント `▲2 ●1`(0件省略・tmux 色マークアップ付き)を出す `vt statusline-summary` に置き換える。dead config だった `agent_badge` を `summary` に整理して実際に配線する。

**Architecture:** daemon Query + list-panes フォールバックの2段構え(現行 agent-badge と同じ)を踏襲。カウントは daemon 側では panes + unread マップから直接集計する(sidebar rows からは数えない — 折りたたみ・フィルタで rows から chat 行が消えるため不正確)。フォールバック側は unread を持たないため done(✓)は idle(○)として数える(既知の劣化として文書化)。

**Tech Stack:** 既存のまま(新規依存なし)

**互換性の注意:** `statusline-agent-badge` サブコマンドと `QueryTarget::Statusline` / `ServerMessage::Statusline` は削除する(後方互換対応はしない方針)。ユーザーの tmux.conf は `vt statusline-summary` への書き換えが必要 — migration.md に明記。

## DoD

### 機能完了条件

- [ ] `vt statusline-summary` が `#[fg=red]▲2#[default] #[fg=green]●1#[default]` 形式(0件の状態は省略、agent 0体なら空文字列)を出力する
- [ ] グリフは `badge.glyphs` を使用。色の既定は blocked=red / working=green / done=cyan / idle=色なし
- [ ] daemon 稼働時は unread を反映した正確なカウント(✓ と ○ を区別)、daemon 停止時は list-panes フォールバック(✓ は ○ に縮退)で動作する
- [ ] `statusline.summary.enabled: false` で空文字列を返す(旧 `agent_badge.enabled` の dead config が解消される)
- [ ] `vt statusline-agent-badge` は存在しない(コマンド・protocol とも削除)

### テスト完了条件

- [ ] `rtk cargo test` 全通過
- [ ] 新規テスト: カウント集計(unread 込み)、0件省略、agent 0体で空、enabled=false で空、フォールバック経路、protocol roundtrip
- [ ] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [ ] `docs/e2e-smoke.md` の `running:1` 期待を summary 形式に更新し、smoke 実施を記録
- [ ] `docs/migration.md` に `statusline-agent-badge` → `statusline-summary` の書き換え(M7 の dotfiles 変更項目)と config キー変更(`statusline.agent_badge` → `statusline.summary`)を追記
- [ ] `docs/statusline-ui-proposals.md` §7.2 Step 2 にチェック

---

## Task 0: summary 描画の純関数と config 整理

**Files:**
- Modify: `src/config/mod.rs`(AgentBadgeConfig → SummaryConfig)
- Modify: `src/daemon/mod.rs`(render_summary 追加、render_agent_badge / rollup_label 削除)

- [ ] **Step 1: 失敗するテストを書く**

`src/daemon/mod.rs` tests:

```rust
#[test]
fn render_summary_counts_states_with_markup_and_omits_zero() {
    use crate::daemon::session_badge::BadgeState;
    let glyphs = crate::config::BadgeGlyphs::default();
    let counts = [
        (BadgeState::Blocked, 2),
        (BadgeState::Working, 1),
        (BadgeState::Done, 0),
        (BadgeState::Idle, 3),
    ];
    assert_eq!(
        render_summary(&counts, &glyphs),
        "#[fg=red]▲2#[default] #[fg=green]●1#[default] ○3"
    );
}

#[test]
fn render_summary_is_empty_without_agents() {
    let glyphs = crate::config::BadgeGlyphs::default();
    let counts = [];
    assert_eq!(render_summary(&counts, &glyphs), "");
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib daemon::tests::render_summary_counts_states_with_markup_and_omits_zero`
Expected: コンパイルエラー

- [ ] **Step 3: 実装**

`src/config/mod.rs`: `AgentBadgeConfig`(130-139行)を削除し、`StatuslineConfig` のフィールドを差し替える:

```rust
pub struct StatuslineConfig {
    pub sessions: StatuslineSessionsConfig,
    pub category: StatuslineCategoryConfig,
    pub summary: SummaryConfig,
    pub session_badge: SessionBadgeConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SummaryConfig {
    pub enabled: bool,
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
```

(`StatuslineConfig` は `#[serde(default)]` のみで deny_unknown_fields なしのため、既存 config の `statusline.agent_badge` キーは黙殺される。migration.md に記載。config の JSON Schema(`src/config/schema.rs`)に statusline 節があれば同期する。)

`src/daemon/mod.rs`: `render_agent_badge`(101-106行)と `rollup_label`(180-189行)を削除し、以下を追加:

```rust
use crate::daemon::session_badge::{BadgeState, glyph_for_state};

pub fn render_summary(counts: &[(BadgeState, usize)], glyphs: &crate::config::BadgeGlyphs) -> String {
    let color = |state: BadgeState| match state {
        BadgeState::Blocked => Some("red"),
        BadgeState::Working => Some("green"),
        BadgeState::Done => Some("cyan"),
        BadgeState::Idle => None,
    };
    counts
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(state, count)| {
            let glyph = glyph_for_state(*state, glyphs);
            match color(*state) {
                Some(color) => format!("#[fg={color}]{glyph}{count}#[default]"),
                None => format!("{glyph}{count}"),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
```

`render_agent_badge` を参照していたテスト(`render_agent_badge_is_empty_without_agents` 等)は削除または summary 版に置換。

- [ ] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`(この時点で protocol / cli 側がコンパイルエラーになる場合は Task 1 と合わせて1コミットにしてよい)

```bash
rtk git add -A
rtk git commit -m "agent badge を状態別カウントの summary 描画に置き換える"
```

---

## Task 1: daemon Query とフォールバックの置換

**Files:**
- Modify: `src/daemon/protocol.rs`(QueryTarget / ServerMessage)
- Modify: `src/daemon/runtime.rs`(QueryStatusline → QuerySummary、カウント集計)
- Modify: `src/daemon/server.rs`(dispatch)
- Modify: `src/daemon/mod.rs`(query / fallback / エントリポイント)
- Modify: `src/cli/mod.rs` / `src/cli/daemon.rs`(サブコマンド差し替え)

- [ ] **Step 1: 失敗するテストを書く**

`src/daemon/runtime.rs` tests:

```rust
#[test]
fn summary_query_counts_unread_as_done() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    // running → idle 遷移で unread(done)を作る
    let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    let _ = state.apply_event(DaemonEvent::PanesUpdated(vec![
        agent_pane("main", "%1", "idle"),
        agent_pane("main", "%2", "running"),
    ]));

    let (reply, receiver) = std::sync::mpsc::channel();
    state.apply_event(DaemonEvent::QuerySummary { reply });
    let message = receiver.recv().unwrap();
    assert_eq!(
        message,
        ServerMessage::Summary {
            text: "#[fg=green]●1#[default] #[fg=cyan]✓1#[default]".to_string()
        }
    );
}
```

`src/daemon/protocol.rs` tests:

```rust
#[test]
fn summary_query_roundtrips() {
    let message = ClientMessage::Query {
        proto: 1,
        what: QueryTarget::Summary,
    };
    let json = serde_json::to_string(&message).unwrap();
    assert_eq!(json, r#"{"op":"query","proto":1,"what":"summary"}"#);
    assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), message);
}
```

- [ ] **Step 2: テストが失敗することを確認 → 実装**

`src/daemon/protocol.rs`:
- `QueryTarget::Statusline` → `QueryTarget::Summary` にリネーム
- `ServerMessage::Statusline { agent_badge }` → `ServerMessage::Summary { text }` にリネーム
- 既存 protocol テスト(`query_statusline_uses_role_declaration_shape` 等)を新形へ更新

`src/daemon/runtime.rs`:
- `DaemonEvent::QueryStatusline` → `QuerySummary`
- ハンドラ(184-193行)を置換:

```rust
            DaemonEvent::QuerySummary { reply } => {
                let text = self.render_summary_text();
                let _ = reply.send(ServerMessage::Summary { text });
                Vec::new()
            }
```

- メソッド追加(unread 込みの正確な集計。sidebar rows からは数えない):

```rust
    fn render_summary_text(&self) -> String {
        use crate::daemon::session_badge::{BadgeState, badge_state};
        if !self.config.statusline.summary.enabled {
            return String::new();
        }
        let mut blocked = 0usize;
        let mut working = 0usize;
        let mut done = 0usize;
        let mut idle = 0usize;
        for pane in self.panes.iter().filter(|pane| is_live_agent_pane(pane)) {
            let level = crate::sidebar::tree::rollup_for_pane(pane);
            let unread = self.unread.get(&pane.pane_id).copied().unwrap_or(false);
            match badge_state(level, unread) {
                BadgeState::Blocked => blocked += 1,
                BadgeState::Working => working += 1,
                BadgeState::Done => done += 1,
                BadgeState::Idle => idle += 1,
            }
        }
        crate::daemon::render_summary(
            &[
                (BadgeState::Blocked, blocked),
                (BadgeState::Working, working),
                (BadgeState::Done, done),
                (BadgeState::Idle, idle),
            ],
            &self.config.badge.glyphs,
        )
    }
```

`src/daemon/server.rs`: Query dispatch(`QueryStatusline` を送っている箇所)と `handle_query_returns_statusline_payload` 系テストを Summary へ更新。

`src/daemon/mod.rs`:
- `query_statusline_agent_badge`(147-168行)→ `query_statusline_summary`(`QueryTarget::Summary` 送信、`ServerMessage::Summary { text }` 受信)
- `statusline_agent_badge_fallback`(108-111行)→ `statusline_summary_fallback`: read_all_panes → 各 pane の `pane_rollup_level` から `badge_state(level, false)` で集計(unread 不明のため done は idle 側に落ちる)→ `render_summary`
- `statusline_agent_badge`(113-124行)→ `statusline_summary`(2段構えの構造は不変)

`src/cli/mod.rs`(248-266行付近)/ `src/cli/daemon.rs`(13-18行): `StatuslineAgentBadge` サブコマンドを `StatuslineSummary`(コマンド名 `statusline-summary`)に差し替え。`src/cli/tests.rs` の `dispatch_statusline_agent_badge_falls_back_to_tmux_snapshot`(58-80行)は summary 版に書き換え(期待値: running 1体なら `"#[fg=green]●1#[default]"`)。

- [ ] **Step 3: テスト通過を確認**

Run: `rtk cargo test`
Expected: 全通過(`statusline-agent-badge` への参照が residual grep でゼロであること: `rtk proxy /usr/bin/grep -rn "agent_badge\|agent-badge\|AgentBadge" src/ docs/e2e-smoke.md` で確認し、docs の歴史的記述以外を掃除)

- [ ] **Step 4: コミット**

```bash
rtk git add -A
rtk git commit -m "statusline-agent-badge を statusline-summary に置き換える"
```

---

## Task 2: smoke・ドキュメント・品質ゲート

- [ ] **Step 1: 品質ゲート**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`

- [ ] **Step 2: smoke**

`docs/e2e-smoke.md` の `vt statusline-agent-badge`(期待 `running:1`)手順を `vt statusline-summary`(期待 `#[fg=green]●1#[default]`)に更新し、scratch tmux で daemon 経由・フォールバック(daemon 停止)両方を確認して記録。

- [ ] **Step 3: migration.md 更新とコミット**

M7 の dotfiles 変更項目に「`#(vtm statusline-agent-badge)` 相当 → `#(vt statusline-summary)`」と config キー変更を追記。

```bash
rtk git add -A
rtk git commit -m "Plan 20 の smoke 結果と docs を更新する"
```
