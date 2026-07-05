# Plan 22: attention チャネルと heartbeat(statusline 再設計 Step 4)

> **実装者向け:** `docs/statusline-ui-proposals.md` §7.2 Step 4 の実装。**Plan 19〜21 完了、および sidebar Plan 15(TRIAGE / runtime の triage 集合)完了が前提**。Task 順に TDD で進める。

**Goal:** 「今 attach していない session の blocked」だけを名指しする `vt statusline-attention` を新設し、daemon 死活の heartbeat + stale 表示、`status-interval` 推奨の文書化で statusline を頑健化する。

**Architecture:** attention は daemon Query + list-panes フォールバックの2段構え(summary と同型)。daemon 側は Plan 15 で導入済みの triage 集合(退出デバウンス済み)を情報源とし、「見えている」判定は unread と同じ `window_active && session_attached` を使う。出力は例外部分のみ(空なら空文字列)とし、summary との合成は tmux.conf 側(`#(vt statusline-attention) #(vt statusline-summary)`)に委ねる。heartbeat は daemon が毎ポーリングで `@vde_heartbeat`(epoch 秒)をグローバル option に書き、`statusline-sessions` が閾値超過時にバッジを `?` に落とす。

**Tech Stack:** 既存のまま(新規依存なし)

## DoD

### 機能完了条件

- [ ] `vt statusline-attention` が、attach されていない(または window 非アクティブな)session の blocked agent を `#[fg=red]▲ {session} · {reason} {elapsed}#[default]` 形式で最も古い1件 + `+N`(2件以上時)出力する。該当なしなら空文字列
- [ ] reason は `perm` / `wait` / `err` の略語(rollup: Permission → perm、Waiting → wait、Error → err)
- [ ] daemon 停止時は list-panes フォールバックで動作する(unread 相当の情報のみ欠落。wait_reason / 経過は出る)
- [ ] daemon が毎ポーリングで `@vde_heartbeat` に epoch 秒を書き、graceful shutdown で消す
- [ ] heartbeat が `max(5秒, poll_ms×3)` より古い(または存在するのに古い)とき、`statusline-sessions` のバッジが `?` に置き換わる。heartbeat option が存在しなければ従来どおり(daemon 未使用運用を壊さない)
- [ ] README に `set -g status-interval 1` 推奨、summary / attention / heartbeat の説明と設定例が載る

### テスト完了条件

- [ ] `rtk cargo test` 全通過
- [ ] 新規テスト: attention の対象選別(可視 session 除外・最古選択・+N)、空出力、reason 略語、フォールバック経路、heartbeat effect の発行、stale 判定(閾値・option 不在)
- [ ] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [ ] `docs/e2e-smoke.md` に attention(permission 発生 → 別 session から見える → 承認で消える)と stale(daemon kill → バッジが `?`)の手順を追記し、smoke 実施を記録
- [ ] README / migration.md 更新(status-interval・設定例・M7 手順への追記)
- [ ] `docs/statusline-ui-proposals.md` §7.2 Step 4 にチェック

---

## Task 0: attention の daemon Query

**Files:**
- Modify: `src/daemon/protocol.rs`(QueryTarget::Attention / ServerMessage::Attention)
- Modify: `src/daemon/runtime.rs`(QueryAttention ハンドラ)
- Modify: `src/daemon/server.rs`(dispatch)

- [ ] **Step 1: 失敗するテストを書く**

`src/daemon/runtime.rs` tests:

```rust
#[test]
fn attention_names_oldest_hidden_blocked_session() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let now = crate::sidebar::tree::now_epoch_secs();
    let mut blocked_old = agent_pane("proxy", "%1", "waiting");
    blocked_old.wait_reason = "permission_prompt".to_string();
    blocked_old.started_at = (now - 120).to_string();
    let mut blocked_new = agent_pane("etl", "%2", "waiting");
    blocked_new.wait_reason = "permission_prompt".to_string();
    blocked_new.started_at = (now - 30).to_string();
    // 可視 session の blocked は対象外
    let mut visible = agent_pane("main", "%3", "waiting");
    visible.wait_reason = "permission_prompt".to_string();
    visible.window_active = true;
    visible.session_attached = true;
    state.apply_event(DaemonEvent::PanesUpdated(vec![
        blocked_old,
        blocked_new,
        visible,
    ]));

    let (reply, receiver) = std::sync::mpsc::channel();
    state.apply_event(DaemonEvent::QueryAttention { reply });
    let ServerMessage::Attention { text } = receiver.recv().unwrap() else {
        panic!("expected attention");
    };
    assert!(text.contains("▲ proxy · perm 2m"), "{text}");
    assert!(text.contains("+1"), "{text}");
    assert!(!text.contains("main"), "{text}");
}

#[test]
fn attention_is_empty_without_hidden_blocked() {
    // 可視 blocked のみ / blocked なし の両ケースで空文字列
}
```

- [ ] **Step 2: テストが失敗することを確認 → 実装**

`src/daemon/protocol.rs`: `QueryTarget` に `Attention`、`ServerMessage` に `Attention { text: String }` を追加(roundtrip テストも)。

`src/daemon/runtime.rs`: `DaemonEvent::QueryAttention { reply }` を追加し、ハンドラ:

```rust
    fn render_attention_text(&self) -> String {
        use crate::hook::RollupLevel;
        let now = crate::sidebar::tree::now_epoch_secs();
        let mut hidden: Vec<(&PaneSnapshot, i64)> = self
            .panes
            .iter()
            .filter(|pane| is_live_agent_pane(pane))
            .filter(|pane| self.triage.contains(&pane.pane_id))
            .filter(|pane| !(pane.window_active && pane.session_attached))
            .map(|pane| {
                let started = pane.started_at.parse::<i64>().unwrap_or(now);
                (pane, (now - started).max(0))
            })
            .collect();
        if hidden.is_empty() {
            return String::new();
        }
        hidden.sort_by(|left, right| right.1.cmp(&left.1)); // 経過が長い順
        let (pane, elapsed) = hidden[0];
        let reason = match crate::sidebar::tree::rollup_for_pane(pane) {
            RollupLevel::Error => "err",
            RollupLevel::Permission => "perm",
            _ => "wait",
        };
        let elapsed = if elapsed < 60 {
            format!("{elapsed}s")
        } else {
            format!("{}m", elapsed / 60)
        };
        let more = hidden.len() - 1;
        let suffix = if more > 0 { format!(" +{more}") } else { String::new() };
        format!(
            "#[fg=red]▲ {} · {reason} {elapsed}{suffix}#[default]",
            pane.session
        )
    }
```

(`self.triage` は sidebar Plan 15 で導入済みの退出デバウンス付き集合。存在しない場合は Plan 15 未完了なので実装を中断してユーザーに報告する。)

`src/daemon/server.rs`: Query dispatch に Attention を追加。

- [ ] **Step 3: テスト通過を確認してコミット**

```bash
rtk git add -A
rtk git commit -m "daemon に attention クエリを追加する"
```

---

## Task 1: CLI とフォールバック

**Files:**
- Modify: `src/daemon/mod.rs`(query_statusline_attention / statusline_attention / fallback)
- Modify: `src/cli/mod.rs`(`statusline-attention` サブコマンド)

- [ ] **Step 1: 失敗するテストを書く**

`src/cli/tests.rs`(summary のフォールバックテストと同じ流儀):

```rust
#[test]
fn dispatch_statusline_attention_falls_back_to_tmux_snapshot() {
    // daemon socket 不在 + モック runner が
    // 非 attach session の permission 待ち pane を返す状態で
    // `vt statusline-attention` の出力が "▲ proxy · perm" を含むことを検証
}
```

- [ ] **Step 2: 実装**

`src/daemon/mod.rs`:
- `query_statusline_attention(socket_path)`: `QueryTarget::Attention` → `ServerMessage::Attention { text }`
- `statusline_attention_fallback(runner)`: `read_all_panes` → `is_live_agent_pane` かつ `badge_state(pane_rollup_level(...), false) == BadgeState::Blocked` かつ `!(window_active && session_attached)` を対象に、runtime 版と同じ整形(triage デバウンスと unread はフォールバックでは効かない — 既知の劣化としてコメント)。整形部分は runtime と重複するため、`fn format_attention(entries: &[(String, RollupLevel, i64)]) -> String` を daemon/mod.rs に置いて runtime からも呼ぶ形に共通化する
- `statusline_attention(runner, env)`: summary と同じ2段構え

`src/cli/mod.rs`: `StatuslineAttention`(コマンド名 `statusline-attention`)を追加し、dispatch する。

- [ ] **Step 3: テスト通過を確認してコミット**

```bash
rtk git add -A
rtk git commit -m "vt statusline-attention を追加する"
```

---

## Task 2: heartbeat と stale 表示

**Files:**
- Modify: `src/options/mod.rs`(KEY_HEARTBEAT)
- Modify: `src/daemon/runtime.rs`(Heartbeat effect)/ `src/daemon/server.rs`(書き込み・shutdown 時クリア)
- Modify: `src/statusline/mod.rs`(stale 判定とバッジ置換)

- [ ] **Step 1: 失敗するテストを書く**

`src/daemon/runtime.rs` tests:

```rust
#[test]
fn panes_updated_emits_heartbeat_effect_when_epoch_advances() {
    // PanesUpdated を2回、同一 epoch 内なら Heartbeat effect は1回だけ
    // (エポック秒が進んだら再発行される)ことを検証
}
```

`src/statusline/mod.rs` tests:

```rust
#[test]
fn stale_heartbeat_replaces_badges_with_question_mark() {
    // heartbeat = now - 60, poll_ms = 1000 → stale
    assert!(is_heartbeat_stale(Some(1000), 1000_i64 * 0 + 60, 1000));
    // 閾値内は fresh
    assert!(!is_heartbeat_stale(Some(1000), 2, 1000));
    // heartbeat option 不在(None)は stale 扱いにしない(daemon 未使用運用)
    // render_statusline_sessions_with_heartbeat で badge "▲" が "?" になることを検証
}
```

(`is_heartbeat_stale(heartbeat_age_secs, ...)` の正確なシグネチャは実装時に determine。検証意図: 閾値 = `max(5, poll_ms * 3 / 1000)` 秒。)

- [ ] **Step 2: 実装**

`src/options/mod.rs`:

```rust
pub const KEY_HEARTBEAT: &str = "@vde_heartbeat";
```

`src/daemon/runtime.rs`:
- `RuntimeEffect::Heartbeat(i64)` を追加
- `RuntimeState` に `last_heartbeat: i64` を持ち、`PanesUpdated` 処理の最後に `now_epoch_secs()` が進んでいたら effect を積む

`src/daemon/server.rs`:
- `Heartbeat(epoch)` → `set-option -g @vde_heartbeat <epoch>`(グローバル option)
- `Shutdown` 時の badge クリアと同じ場所で `set-option -gu @vde_heartbeat`(クリア)

`src/statusline/mod.rs`:
- `statusline_sessions` で heartbeat を1回読む(`runner.run(&["show-options", "-gqv", "@vde_heartbeat"])`)。値が存在し、`now - heartbeat > max(5, poll_ms*3/1000)` なら、各 session の badge を `?`(状態不明)に置換して描画する。option が空(daemon 未起動 or graceful shutdown 済み)なら従来どおり(badge は書かれていないので自然に無印)
- 判定は純関数 `is_heartbeat_stale(...)` に切り出してテスト可能にする。`poll_ms` は `config.daemon.poll_ms`

- [ ] **Step 3: テスト通過を確認してコミット**

```bash
rtk git add -A
rtk git commit -m "daemon heartbeat と stale バッジ表示を追加する"
```

---

## Task 3: README・smoke・品質ゲート

- [ ] **Step 1: README 更新**

statusline 節を新設(または更新)し、以下を記載:
- `set -g status-interval 1` 推奨と、反映遅延が `daemon.poll_ms + status-interval` の合成である説明
- 設定例:

```tmux
set -g status-interval 1
set -g status-left '#(vt statusline-category)#(vt statusline-sessions --show-index)'
set -g status-right '#(vt statusline-attention) #(vt statusline-summary)'
```

- `badge_style` / `hide_idle` / `{count}` / summary / attention の config 例

- [ ] **Step 2: 品質ゲート**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`

- [ ] **Step 3: smoke**

scratch tmux で(daemon 再起動込み):
- 別 session で permission 待ちを発生 → `vt statusline-attention` に `▲ {session} · perm {経過}` が出る → 承認で消える(退出デバウンス約2秒)
- 可視 session の permission は attention に出ない
- daemon を kill → 約5秒後に `statusline-sessions` のバッジが `?` になる → daemon 再起動で復帰
- daemon graceful shutdown → heartbeat が消え、バッジも消えて `?` は出ない

結果を `docs/e2e-smoke.md` に追記。

- [ ] **Step 4: コミット**

```bash
rtk git add -A
rtk git commit -m "Plan 22 の smoke 結果と statusline docs を更新する"
```
