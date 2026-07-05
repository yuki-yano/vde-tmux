# Plan 19: statusline 表示基盤(statusline 再設計 Step 1)

> **実装者向け:** `docs/statusline-ui-proposals.md` §7.2 Step 1 の実装。**sidebar Plan 13〜17 と Plan 18(sidebar 修正)完了が前提**。Task 順に TDD(失敗テスト → 実装 → 全テスト → コミット)で進める。

**Goal:** session badge の suffix 既定を空にし、idle バッジの表示制御(`hide_idle`、既定は表示)と current session の既定スタイルを導入する。単幅グリフ(▲●✓○、Plan 13 で導入済み)を statusline で違和感なく使える土台を作る。

**Architecture:** すべて既存の経路(daemon が `@vde_session_status` を書き、`statusline-sessions` が list-sessions 経由で読む)の上のパラメータ変更・小拡張。新しいデータ経路は作らない。

**Tech Stack:** 既存のまま(新規依存なし)

## DoD

### 機能完了条件

- [x] session badge の既定 suffix が `""` になり、`▲main` のようにグリフとラベルが `{badge}{label}` で自然に連結される(format 側の空白で調整可能)
- [x] idle(○)バッジが既定で表示される。`statusline.session_badge.hide_idle: true` で非表示にできる(無印 = agent なし、に純化)
- [x] 既定 config で current session セグメントが bold で描画され、other と視覚的に区別できる
- [x] 既存の pill 運用(dotfiles の prefix/suffix/colors)が無変更で動く

### テスト完了条件

- [x] `rtk cargo test` 全通過(suffix 既定変更に伴う期待値更新を含む)
- [x] 新規テスト: hide_idle の true/false、current 既定 bold の出力
- [x] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [x] `docs/e2e-smoke.md` の statusline 期待値(バッジ + suffix)を更新し、scratch tmux で smoke を実施して記録
- [x] `docs/migration.md` に suffix 既定変更の注意(空白を維持したい場合は `session_badge.suffix: " "` を明示)を追記
- [x] `docs/statusline-ui-proposals.md` §7.2 Step 1 にチェック

---

## Task 0: suffix 既定を空にする

**Files:**
- Modify: `src/config/mod.rs`(SessionBadgeConfig::default)
- Modify: `src/daemon/session_badge.rs` / `src/daemon/runtime.rs` の期待値テスト

- [x] **Step 1: 失敗するテストを書く(既存テストの期待値を先に更新する)**

`src/daemon/session_badge.rs` の `session_rollup_picks_most_urgent_state`(88-97行):

```rust
        assert_eq!(value.as_deref(), Some("▲"));
```

(`"▲ "` → `"▲"`。)

`src/daemon/runtime.rs` の session badge 期待値(`running_to_idle_becomes_done_until_window_viewed` 等、`"● "` / `"✓ "` / `"○ "` / `"▲ "` を期待しているテストすべて)を suffix なしに更新する。

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib daemon::session_badge`
Expected: FAIL(現行 suffix は `" "`)

- [x] **Step 3: 実装**

`src/config/mod.rs` の `SessionBadgeConfig::default`(143-150行付近)を変更:

```rust
impl Default for SessionBadgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            suffix: String::new(),
        }
    }
}
```

絵文字幅対策のコメント(「絵文字の直後に…」)は単幅グリフ化で不要になったため削除する。ただし `render_session_segment` は `{badge}{label}` 連結のため、既定ではグリフとセッション名が密着する — statusline の見た目上の区切りは `SegmentStyle.format`(既定 `" {session} "`)の空白が担う。バッジとラベルの間に空白が欲しい場合の設定例を migration.md に書く(Task 2)。

**注意**: `render_session_segment`(statusline/mod.rs:146)は badge が空文字なら `{label}` のみになる。suffix を空にすると「badge あり」と「badge なし」で label 開始位置が1セルずれる。これを嫌う場合の逃げ道が `suffix: " "` 復活であることも migration.md に明記。

- [x] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "session badge の suffix 既定を空にする"
```

---

## Task 1: hide_idle オプション

**Files:**
- Modify: `src/config/mod.rs`(SessionBadgeConfig)
- Modify: `src/daemon/session_badge.rs`(session_badge_value)
- Modify: `src/daemon/runtime.rs`(sync_session_badges の呼び出し)

- [x] **Step 1: 失敗するテストを書く**

`src/daemon/session_badge.rs` tests:

```rust
#[test]
fn hide_idle_suppresses_idle_badge_only() {
    let glyphs = BadgeGlyphs::default();
    // hide_idle: idle は None(バッジ消去)
    assert_eq!(
        session_badge_value([BadgeState::Idle], &glyphs, "", true),
        None
    );
    // hide_idle でも blocked/working/done は出る
    assert_eq!(
        session_badge_value([BadgeState::Done], &glyphs, "", true).as_deref(),
        Some("✓")
    );
    // 既定(false)では idle も出る
    assert_eq!(
        session_badge_value([BadgeState::Idle], &glyphs, "", false).as_deref(),
        Some("○")
    );
}
```

`src/daemon/runtime.rs` tests:

```rust
#[test]
fn hide_idle_config_clears_idle_session_badge() {
    let mut config = Config::default();
    config.statusline.session_badge.hide_idle = true;
    let mut state = RuntimeState::new(config, SidebarState::default());
    let effects = state.apply_event(DaemonEvent::PanesUpdated(vec![agent_pane(
        "main", "%1", "idle",
    )]));
    // idle のみの session にはバッジを書かない
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        RuntimeEffect::SetSessionBadge { .. }
    )));
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib daemon::session_badge::tests::hide_idle_suppresses_idle_badge_only`
Expected: コンパイルエラー(引数が3つ)

- [x] **Step 3: 実装**

`src/config/mod.rs` の `SessionBadgeConfig` に追加(`#[serde(default)]` 構造体なので追加は非破壊):

```rust
pub struct SessionBadgeConfig {
    pub enabled: bool,
    pub suffix: String,
    pub hide_idle: bool,
}
```

(`Default` impl に `hide_idle: false` を追加。)

`src/daemon/session_badge.rs` の `session_badge_value`(33-40行)を変更:

```rust
pub fn session_badge_value(
    states: impl IntoIterator<Item = BadgeState>,
    glyphs: &BadgeGlyphs,
    suffix: &str,
    hide_idle: bool,
) -> Option<String> {
    let state = states.into_iter().min()?;
    if hide_idle && state == BadgeState::Idle {
        return None;
    }
    Some(format!("{}{suffix}", glyph_for_state(state, glyphs)))
}
```

呼び出し側(`src/daemon/runtime.rs` の `sync_session_badges`、`session_badge_value(list, badge_glyphs, &badge_config.suffix)` の箇所)に `badge_config.hide_idle` を渡す。既存テストの呼び出しにも第4引数 `false` を追加(コンパイルエラーで全箇所検出される)。

- [x] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "session badge に hide_idle オプションを追加する"
```

---

## Task 2: current session の既定スタイルと docs

**Files:**
- Modify: `src/config/mod.rs`(StatuslineSessionsConfig::default)
- Modify: `src/statusline/mod.rs` tests
- Modify: `docs/migration.md` / `docs/e2e-smoke.md`

- [x] **Step 1: 失敗するテストを書く**

`src/statusline/mod.rs` tests:

```rust
#[test]
fn current_session_is_bold_by_default() {
    let config = Config::default();
    let rendered = render_statusline_sessions(
        &config,
        &[session("main", "work"), session("sub", "work")],
        "main",
        "work",
    );
    // current(main)は #[bold] で包まれ、other(sub)は素のまま
    assert!(rendered.contains("#[bold] main #[default]"), "{rendered}");
    assert!(rendered.contains(" sub "), "{rendered}");
    assert!(!rendered.contains("#[bold] sub"), "{rendered}");
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib statusline::tests::current_session_is_bold_by_default`
Expected: FAIL(現行は current/other とも無スタイル)

- [x] **Step 3: 実装**

`src/config/mod.rs` の `StatuslineSessionsConfig`(65-71行)から `Default` derive を外し、手書き impl にする(63-64行の clippy 対策コメントは「current 既定が derive と一致しなくなったため手書きに戻した」旨へ書き換え):

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct StatuslineSessionsConfig {
    pub show_index: bool,
    pub current: SegmentStyle,
    pub other: SegmentStyle,
}

impl Default for StatuslineSessionsConfig {
    fn default() -> Self {
        Self {
            show_index: false,
            current: SegmentStyle {
                bold: true,
                ..SegmentStyle::default()
            },
            other: SegmentStyle::default(),
        }
    }
}
```

**設計判断(§7 からの明確化)**: §7.2 は「bold + 色」としているが、既定の色付けは状態4色(赤緑シアン)や装飾 pill との衝突リスクがあるため **既定は bold のみ**とする。色は `sessions.current.colors.fg` で従来どおり設定可能。pill 運用者は dotfiles 側で current pill を塗っているため実害なし。この判断を実装ノートとして本ファイル末尾に記録する。

- [x] **Step 4: docs 更新**

- `docs/migration.md`: suffix 既定変更の注意(旧挙動維持は `session_badge.suffix: " "`)、hide_idle の紹介
- `docs/e2e-smoke.md`: statusline 期待値を新表示(`▲1:main` 形式・suffix なし)に更新

- [x] **Step 5: テスト・品質ゲート・smoke**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`

scratch tmux で: idle session に ○ が付く / `hide_idle: true` で消える / current が bold / 既存 pill 設定が壊れない、を確認して e2e-smoke.md に記録。

- [x] **Step 6: コミット**

```bash
rtk git add -A
rtk git commit -m "current session を既定 bold にし statusline docs を更新する"
```

## 実装ノート

- §7.2 の「bold + 色」は、既定値としては bold のみで実装した。状態4色や既存 pill 装飾との衝突を避けるため、色は従来どおり `statusline.sessions.current.colors.fg/bg` の明示設定に任せる。
- scratch tmux smoke では、tmux の `pane_current_command` が実 agent 名でないと daemon の live agent 判定に入らないため、scratch 専用の `codex` という sleep 実行ファイルを作って確認した。
