# Plan 21: 案A-lite — inline badge と category 件数(statusline 再設計 Step 3)

> **実装者向け:** `docs/statusline-ui-proposals.md` §7.2 Step 3 の実装。**Plan 19・20 完了が前提**。Task 順に TDD で進める。

**Goal:** session badge を pill 内で状態色付きで描ける `badge_style: inline`(既定)を導入し、category セグメントに `{count}` プレースホルダを追加する。これで提案書の「案A-lite」が完成する。

**Architecture:** バッジの色付けには session の BadgeState が renderer 側で必要になる。グリフ文字列からの逆引き(hacky)を避け、**daemon が `@vde_session_status`(グリフ)と並行して `@vde_session_state`(`blocked|working|done|idle` の構造化値)を書く**経路を新設する。これは Step 5 で category `{badge}` を再評価する際の土台にもなる。tmux 色マークアップは「`#[fg=<状態色>]グリフ#[fg=<セグメントfg or default>]`」で fg だけを切り替え、セグメントの bold/bg には触れない(pill 装飾を壊さない)。

**Tech Stack:** 既存のまま(新規依存なし)

## DoD

### 機能完了条件

- [x] daemon が session ごとに `@vde_session_state` を書き、badge クリア時は両キーとも消える
- [x] `badge_style: inline`(既定)で、バッジグリフがセグメント内で状態色(blocked=red / working=green / done=cyan / idle=色なし)の fg で描かれ、直後にセグメントの fg(未設定なら default)へ復帰する。bold / bg は維持される
- [x] `badge_style: plain` で従来どおり無色のグリフ連結になる
- [x] category の `format` で `{count}`(そのカテゴリの session 数)が使える
- [x] 既存の pill 運用(prefix/suffix/colors)が inline でも壊れない

### テスト完了条件

- [x] `rtk cargo test` 全通過
- [x] 新規テスト: state option の書き込み/クリア、SessionInfo.state のパース、inline のマークアップ(fg あり/なしセグメント両方)、plain の従来出力、{count} 置換
- [x] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [x] `docs/e2e-smoke.md` に inline バッジと {count} の確認を追記し、smoke 実施を記録
- [x] README(または migration.md)に `badge_style` と `{count}` の設定例を追記
- [x] `docs/statusline-ui-proposals.md` §7.2 Step 3 にチェック

---

## Task 0: 構造化状態 `@vde_session_state` の配信

**Files:**
- Modify: `src/options/mod.rs`(KEY_SESSION_STATE 追加)
- Modify: `src/daemon/runtime.rs`(RuntimeEffect::SetSessionBadge に state を追加、sync_session_badges)
- Modify: `src/daemon/server.rs`(effect 処理で両キー書き込み/クリア)
- Modify: `src/session/mod.rs`(session_list_format / parse_sessions / SessionInfo.state)

- [x] **Step 1: 失敗するテストを書く**

`src/daemon/runtime.rs` tests(既存の `panes_updated_emits_set_session_badge_effect` を拡張):

```rust
#[test]
fn session_badge_effect_carries_structured_state() {
    let mut state = RuntimeState::new(Config::default(), SidebarState::default());
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "running",
    )]));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        RuntimeEffect::SetSessionBadge { session, value, state }
            if session == "main" && value.starts_with('●') && state == "working"
    )));
}
```

`src/session/mod.rs` tests(既存 parse テストの流儀で):

```rust
#[test]
fn parse_sessions_reads_state_field() {
    // session_list_format のフィールド数に合わせた1行を作り、
    // 末尾に "working" を置いて SessionInfo.state == "working" を検証
}
```

- [x] **Step 2: テストが失敗することを確認 → 実装**

`src/options/mod.rs`: `KEY_SESSION_STATUS` の隣に追加:

```rust
pub const KEY_SESSION_STATE: &str = "@vde_session_state";
```

`src/daemon/runtime.rs`:
- `RuntimeEffect::SetSessionBadge { session, value }` に `state: String` を追加
- `sync_session_badges` で `session_badge_value` と同時に `BadgeState` の min を文字列化(`blocked|working|done|idle` の小文字。`BadgeState` に `fn as_str(&self) -> &'static str` を session_badge.rs へ追加)して effect に載せる。`written_badges` の比較値は `(value, state)` のタプルに変更

`src/daemon/server.rs` の effect 処理(354-369行付近):

```rust
            RuntimeEffect::SetSessionBadge { session, value, state } => {
                worker_io.set_session_option(&session, crate::options::KEY_SESSION_STATUS, &value)?;
                worker_io.set_session_option(&session, crate::options::KEY_SESSION_STATE, &state)?;
            }
            RuntimeEffect::ClearSessionBadge { session } => {
                worker_io.unset_session_option(&session, crate::options::KEY_SESSION_STATUS)?;
                worker_io.unset_session_option(&session, crate::options::KEY_SESSION_STATE)?;
            }
```

(実際のエラーハンドリングは既存コードの形に合わせる。)

`src/session/mod.rs`:
- `session_list_format()`(31-40行)の末尾に `#{@vde_session_state}` フィールドを追加
- `parse_sessions`(44-63行)で新フィールドを `SessionInfo.state: String` に格納(`SessionInfo` にフィールド追加、`Default` 対応)
- フィールド数変更に伴う既存 parse テストの更新

既存 runtime テストの `RuntimeEffect::SetSessionBadge { .. }` パターンマッチはフィールド追加でコンパイルエラーになるため全箇所更新する。

- [x] **Step 3: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "session badge と並行して構造化状態 option を配信する"
```

---

## Task 1: badge_style(inline / plain)

**Files:**
- Modify: `src/config/mod.rs`(StatuslineSessionsConfig.badge_style)
- Modify: `src/statusline/mod.rs`(render_session_segment / tmux_style_segment)

- [x] **Step 1: 失敗するテストを書く**

`src/statusline/mod.rs` tests:

```rust
#[test]
fn inline_badge_uses_state_color_and_restores_segment_fg() {
    let config = Config::default(); // badge_style は既定 inline、current は bold
    let mut main = session("main", "work");
    main.badge = "▲".to_string();
    main.state = "blocked".to_string();
    let rendered = render_statusline_sessions(&config, &[main], "main", "work");
    // セグメント fg 未設定 → グリフ後は fg=default で復帰(bold は #[bold] のまま維持)
    assert!(
        rendered.contains("#[fg=red]▲#[fg=default]main"),
        "{rendered}"
    );
}

#[test]
fn inline_badge_restores_configured_segment_fg() {
    let mut config = Config::default();
    config.statusline.sessions.other.colors.fg = Some("white".to_string());
    let mut sub = session("sub", "work");
    sub.badge = "●".to_string();
    sub.state = "working".to_string();
    let rendered = render_statusline_sessions(&config, &[sub], "main", "work");
    assert!(
        rendered.contains("#[fg=green]●#[fg=white]sub"),
        "{rendered}"
    );
}

#[test]
fn plain_badge_style_keeps_legacy_concatenation() {
    let mut config = Config::default();
    config.statusline.sessions.badge_style = BadgeStyle::Plain;
    let mut main = session("main", "work");
    main.badge = "▲".to_string();
    main.state = "blocked".to_string();
    let rendered = render_statusline_sessions(&config, &[main], "main", "work");
    assert!(rendered.contains("▲main"), "{rendered}");
    assert!(!rendered.contains("#[fg=red]"), "{rendered}");
}
```

- [x] **Step 2: テストが失敗することを確認 → 実装**

`src/config/mod.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BadgeStyle {
    #[default]
    Inline,
    Plain,
}
```

`StatuslineSessionsConfig` に `pub badge_style: BadgeStyle` を追加(Default impl に `badge_style: BadgeStyle::Inline`)。

`src/statusline/mod.rs`:
- `render_session_segment`(134-152行)を拡張。badge 部分を `{badge}{label}` の単純連結から、inline 時は色マークアップ付き断片に変える:

```rust
fn badge_fragment(badge: &str, state: &str, style: &SegmentStyle, badge_style: BadgeStyle) -> String {
    if badge.is_empty() {
        return String::new();
    }
    if badge_style == BadgeStyle::Plain {
        return badge.to_string();
    }
    let color = match state {
        "blocked" => Some("red"),
        "working" => Some("green"),
        "done" => Some("cyan"),
        _ => None, // idle・不明は色なし
    };
    match color {
        Some(color) => {
            let restore = style.colors.fg.as_deref().unwrap_or("default");
            format!("#[fg={color}]{badge}#[fg={restore}]")
        }
        None => badge.to_string(),
    }
}
```

`render_session_segment` は `format!("{badge_fragment}{label}")` を `{session}` に埋める。呼び出し側(`render_statusline_sessions`)から `session.state` と `config.statusline.sessions.badge_style` を渡すようシグネチャを拡張する。

**設計注意**: fg のみを切り替え、`#[default]` は使わない(セグメントの bold/bg を巻き添えでリセットしないため)。外側の `tmux_style_segment` が最後に `#[default]` で全体を閉じる構造は不変。

- [x] **Step 3: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "badge_style inline で状態色付きバッジを描画する"
```

---

## Task 2: category {count}

**Files:**
- Modify: `src/statusline/mod.rs`(render_statusline_category)

- [x] **Step 1: 失敗するテストを書く**

`src/statusline/mod.rs` tests:

```rust
#[test]
fn category_format_supports_count_placeholder() {
    let mut config = Config::default();
    config.statusline.category.format = "{category} {count} ".to_string();
    let rendered = render_statusline_category(
        &config,
        &[
            session("a", "work"),
            session("b", "work"),
            session("c", "private"),
        ],
        "work",
    );
    assert!(rendered.contains("work 2"), "{rendered}");
    assert!(rendered.contains("private 1"), "{rendered}");
}
```

- [x] **Step 2: テストが失敗することを確認 → 実装**

`render_statusline_category`(90-126行)の body 構築に `{count}` 置換を追加:

```rust
            let count = sessions_in_category(config, sessions, category).len();
            let body = config
                .statusline
                .category
                .format
                .replace("{category}", label)
                .replace("{count}", &count.to_string());
```

既定 format(`"{category} "`)は `{count}` を含まないため挙動不変。

- [x] **Step 3: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "category format に {count} プレースホルダを追加する"
```

---

## Task 3: smoke・ドキュメント・品質ゲート

- [x] **Step 1: 品質ゲート**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`

- [x] **Step 2: smoke**

scratch tmux で確認(daemon 再起動込み):
- `@vde_session_state` が書かれ、badge クリアで両キー消える(`rtk proxy tmux show-options -t <session>` で確認)
- pill 設定(bold + bg)のセグメントで inline バッジが状態色 → セグメント色に正しく復帰し、pill が壊れない
- `badge_style: plain` で従来表示
- `format: "{category} {count} "` で件数が出る

結果を `docs/e2e-smoke.md` に追記。

- [x] **Step 3: docs 更新とコミット**

README(または migration.md)に badge_style / {count} の設定例。`docs/statusline-ui-proposals.md` §7.2 Step 3 にチェック。

```bash
rtk git add -A
rtk git commit -m "Plan 21 の smoke 結果と docs を更新する"
```

## 実装ノート

- 計画からの逸脱なし。
- `badge_style: inline` は fg のみを状態色へ切り替え、直後にセグメント fg へ復帰する。`#[default]` はセグメント末尾の既存リセットに任せ、bold/bg を途中で壊さない。
