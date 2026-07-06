# Plan 25: statusline D3改配色（signal fg ＋ 矩形 accent-active）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> - 配色の設計根拠・モックアップ・実測コントラスト比は `docs/statusline-color-proposals.html` を参照（本 plan はその「D3改」を実装する）。
> - shell command は必ず `rtk` を prefix する（例: `rtk cargo test`）。
> - 着手時点で working tree に `src/project/mod.rs` の無関係な未コミット差分（+101行）が残っている場合がある。触らず、本 plan のコミットに混ぜないこと。
> - 後方互換 fallback は作らない（このリポジトリの方針）。既定値変更はテストも既定値に合わせて書き換える。

**Goal:** ステータスラインの状態グリフ色を config 化して D3改の hex 既定値に置き換え、カレント session のグリフを塗りの外（バー地）に出す `badge_style = "outer"` を追加し、category のアクティブ時ラベル展開を可能にする。

**Architecture:** 信号グリフ（▲●✓○）は常に暗いバー地の上に fg で描き、塗り（インディゴ矩形）はカレント/アクティブ要素の「名前」だけに使う。グリフ色は `badge.colors`（新設）に一元化し、sessions・category badge・summary の3消費箇所すべてが同じ値を参照する。window list・breadcrumb・バー地色は tmux.conf の責務なので、Rust 変更ではなく README のスニペット更新で反映する。

**Tech Stack:** Rust / serde（config）/ tmux フォーマット文字列。テストは各モジュール内 `#[cfg(test)] mod tests`、品質ゲートは `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`。

---

## DoD

### 機能完了条件

- [ ] `badge.colors.{blocked,working,done,idle}` が config で設定でき、既定値が `#ff6b6b` / `#4fd08a` / `#45cbe6` / `#6f6b85` である
- [ ] sessions バッジ・category バッジ・summary の3箇所すべてが `badge.colors` を参照し、ANSI 名前色（`"red"` 等）のハードコードが `src/` から消えている
- [ ] `statusline.sessions.badge_style = "outer"` で、グリフが `#[fg=<状態色>]{glyph}#[default]` としてセグメント塗りの外側（バー地）に描画される
- [ ] idle グリフも含めて全状態が色付きで描画される（従来の「idle は無色」をやめる）
- [ ] `statusline.category.inactive_format`（既定 `"{category} "`）と `{name}` トークンにより、アクティブ category のみラベル展開（例: `🏠 home`）ができる
- [ ] `statusline.attention` の既定 fg が `red` から `#ff6b6b` に変わっている
- [ ] config JSON schema が新フィールドと `badge_style` の `"outer"` を受理する

### テスト完了条件

- [ ] 本 plan の各 Task に定義した新規テストがすべて green
- [ ] 既定値変更の影響を受ける既存テスト（`inline_badge_segment_renders_exact_markup`、`category_badge_shows_worst_state_with_color_and_restore`、`category_badge_is_empty_without_agent_state_and_idle_is_plain`、`attention_segment_defaults_to_red_text`、`render_summary_*` 3件）が新しい既定値で更新され green
- [ ] `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test` がすべて通過

### 運用反映条件

- [ ] README の statusline 節に D3改の config 例（`badge_style = "outer"`・矩形塗り・`inactive_format`）と tmux.conf スニペット（`status-style bg=#1a1926`、window-status 2種、truecolor overrides、`status-left-length`）が追記されている
- [ ] breadcrumb を使う場合の地色再調整（`#121218` 目安）と truecolor 前提が README に明記されている
- [ ] `docs/plans/2026-07-05-statusline-redesign-roadmap.md` に Plan 25 への参照が1行追加されている
- [ ] tmux 実機で `vt statusline-sessions` / `vt statusline-category` / `vt statusline-summary` の出力を目視確認し、結果を本 plan の「実装ノート」に記録している

---

## Task 1: `badge.colors` config の新設

**Files:**
- Modify: `src/config/mod.rs`（`BadgeConfig` / `BadgeGlyphs` 周辺）
- Modify: `src/config/schema.rs:34-49`（`badge` の properties）
- Test: `src/config/mod.rs` 内 `#[cfg(test)] mod tests`

- [ ] **Step 1: 失敗するテストを書く**

`src/config/mod.rs` のテスト mod に追加:

```rust
#[test]
fn badge_colors_default_to_d3_hex() {
    let config = Config::default();
    assert_eq!(config.badge.colors.blocked, "#ff6b6b");
    assert_eq!(config.badge.colors.working, "#4fd08a");
    assert_eq!(config.badge.colors.done, "#45cbe6");
    assert_eq!(config.badge.colors.idle, "#6f6b85");
    assert_eq!(config.badge.colors.for_state("working"), Some("#4fd08a"));
    assert_eq!(config.badge.colors.for_state("unknown"), None);
}
```

- [ ] **Step 2: 失敗を確認する（RED）**

Run: `rtk cargo test badge_colors_default_to_d3_hex`
Expected: FAIL（`colors` フィールド未定義のコンパイルエラー）

- [ ] **Step 3: 実装する（GREEN）**

`src/config/mod.rs` の `BadgeConfig` を拡張し、`BadgeGlyphs` の近くに `BadgeColors` を追加:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct BadgeConfig {
    pub glyphs: BadgeGlyphs,
    pub colors: BadgeColors,
}

/// 状態グリフの描画色。sessions バッジ・category バッジ・summary で共通。
/// 既定値は docs/statusline-color-proposals.html の D3改 signal palette。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct BadgeColors {
    pub blocked: String,
    pub working: String,
    pub done: String,
    pub idle: String,
}

impl Default for BadgeColors {
    fn default() -> Self {
        Self {
            blocked: "#ff6b6b".to_string(),
            working: "#4fd08a".to_string(),
            done: "#45cbe6".to_string(),
            idle: "#6f6b85".to_string(),
        }
    }
}

impl BadgeColors {
    /// 状態文字列に対応する色。未知状態（stale の "" など）は None。
    pub fn for_state(&self, state: &str) -> Option<&str> {
        match state {
            "blocked" => Some(self.blocked.as_str()),
            "working" => Some(self.working.as_str()),
            "done" => Some(self.done.as_str()),
            "idle" => Some(self.idle.as_str()),
            _ => None,
        }
    }
}
```

`src/config/schema.rs` の `badge.properties` に `colors` を追加（`glyphs` と同階層）:

```json
"colors": {
    "type": "object",
    "additionalProperties": true,
    "properties": {
        "blocked": { "type": "string" },
        "working": { "type": "string" },
        "done": { "type": "string" },
        "idle": { "type": "string" }
    }
}
```

- [ ] **Step 4: テスト通過と YAML 上書きを確認する**

YAML から上書きできることのテストも追加（既存の config parse テストの書式に合わせる。`src/config/mod.rs:628` 付近の `display_names` テストが参考）:

```rust
#[test]
fn badge_colors_can_be_overridden_in_yaml() {
    let config = parse_config_str(
        r#"
badge:
  colors:
    working: "#00ff00"
"#,
    )
    .unwrap();
    assert_eq!(config.badge.colors.working, "#00ff00");
    assert_eq!(config.badge.colors.blocked, "#ff6b6b");
}
```

※ `parse_config_str` 相当のヘルパ名は既存テストで使われているものに合わせること（既存 config テストが `serde_yaml_ng::from_str::<Config>` を直接使っているならそれに倣う）。

Run: `rtk cargo test badge_colors`
Expected: PASS（2件）

- [ ] **Step 5: コミット**

```bash
rtk git add src/config/mod.rs src/config/schema.rs
rtk git commit -m "badge.colors を新設して状態グリフ色を config 化する"
```

---

## Task 2: sessions / category バッジを `badge.colors` に切り替える

**Files:**
- Modify: `src/statusline/mod.rs:187-229`（`category_badge_fragment`）
- Modify: `src/statusline/mod.rs:274-334`（`render_session_segment` / `badge_fragment`）
- Modify: `src/statusline/mod.rs:86-127`（呼び出し側 `render_statusline_sessions_with_stale`）
- Test: `src/statusline/mod.rs` 内 `#[cfg(test)] mod tests`

- [ ] **Step 1: 失敗するテストを書く**

既存の完全一致テストを新既定値に書き換える（これが RED になる）。`src/statusline/mod.rs:467-475` の `inline_badge_segment_renders_exact_markup` を:

```rust
#[test]
fn inline_badge_segment_renders_exact_markup() {
    // （既存のセットアップはそのまま）
    assert_eq!(rendered, "#[bold] #[fg=#ff6b6b]▲#[fg=default]main #[default]");
}
```

同様に `category_badge_shows_worst_state_with_color_and_restore`（:586）の期待値の `fg=red` 等を `fg=#ff6b6b` 等に、`category_badge_is_empty_without_agent_state_and_idle_is_plain`（:603）は「idle も色付き」に意味が変わるためテスト名ごと変更:

```rust
#[test]
fn category_badge_is_empty_without_agent_state_and_idle_is_colored() {
    // 前半（agent 状態なし → 空）は既存のまま。
    // 後半: idle は plain ではなく #[fg=#6f6b85] で描画されることを検証する。
    assert!(rendered.contains("#[fg=#6f6b85]"), "{rendered}");
}
```

- [ ] **Step 2: 失敗を確認する（RED）**

Run: `rtk cargo test --lib statusline`
Expected: 上記3テストが FAIL（実装はまだ `"red"`/`"green"` を出力）

- [ ] **Step 3: 実装する（GREEN）**

`badge_fragment`（:309-334）をハードコード match から `BadgeColors::for_state` に置き換える:

```rust
fn badge_fragment(
    badge: &str,
    state: &str,
    style: &SegmentStyle,
    badge_style: BadgeStyle,
    colors: &crate::config::BadgeColors,
) -> String {
    if badge.is_empty() {
        return String::new();
    }
    if badge_style == BadgeStyle::Plain {
        return badge.to_string();
    }
    match colors.for_state(state) {
        Some(color) => {
            let restore = style.colors.fg.as_deref().unwrap_or("default");
            format!("#[fg={color}]{badge}#[fg={restore}]")
        }
        None => badge.to_string(),
    }
}
```

`category_badge_fragment`（:187-229）の `match state { "blocked" => Some("red"), ... }` ブロック（:216-221）を削除し:

```rust
    match config.badge.colors.for_state(state) {
        Some(color) => {
            let restore = colors.fg.as_deref().unwrap_or("default");
            format!("#[fg={color}]{glyph}#[fg={restore}]")
        }
        None => glyph.to_string(),
    }
```

`render_session_segment`（:274-307）に `colors: &crate::config::BadgeColors` 引数を追加して `badge_fragment` へ渡し、呼び出し側 `render_statusline_sessions_with_stale`（:108-116）で `&config.badge.colors` を渡す。

**注意（restore 懸念）:** `#[fg={restore}]` は fg しか復元しない現行仕様を維持する。bg はセグメントの `#[...]` ブロック内で有効なままなので Inline では問題ない。Outer（Task 3）はこの経路を通らない。

- [ ] **Step 4: テスト通過を確認する**

Run: `rtk cargo test --lib statusline`
Expected: PASS（既存テスト全件＋書き換えた3件）

- [ ] **Step 5: コミット**

```bash
rtk git add src/statusline/mod.rs
rtk git commit -m "statusline のグリフ色を badge.colors 参照に切り替える"
```

---

## Task 3: `BadgeStyle::Outer` — グリフを塗りの外に出す

**Files:**
- Modify: `src/config/mod.rs`（`BadgeStyle` enum）
- Modify: `src/config/schema.rs:65-68`（`badge_style` の enum）
- Modify: `src/statusline/mod.rs:274-307`（`render_session_segment`）
- Test: `src/statusline/mod.rs` 内 `#[cfg(test)] mod tests`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn outer_badge_places_glyph_on_bar_before_segment() {
    let mut config = Config::default();
    config.statusline.sessions.badge_style = BadgeStyle::Outer;
    config.statusline.sessions.current.colors.fg = Some("#ecebff".to_string());
    config.statusline.sessions.current.colors.bg = Some("#453f9e".to_string());
    let sessions = vec![session_with_badge("main", "work", "●", "working")];
    let rendered = render_statusline_sessions(&config, &sessions, "main", "work");
    // グリフはセグメント塗りの外（#[default] で地色）に置かれ、名前だけが塗られる
    assert!(
        rendered.contains("#[fg=#4fd08a]●#[default] #[bold,fg=#ecebff,bg=#453f9e] main #[default]"),
        "{rendered}"
    );
}

#[test]
fn outer_badge_without_badge_renders_segment_only() {
    let mut config = Config::default();
    config.statusline.sessions.badge_style = BadgeStyle::Outer;
    let sessions = vec![session("main", "work")]; // badge なし
    let rendered = render_statusline_sessions(&config, &sessions, "main", "work");
    assert!(!rendered.contains("#[default] #[bold]"), "{rendered}");
    assert!(rendered.contains("#[bold] main #[default]"), "{rendered}");
}
```

※ `session_with_badge` ヘルパが無ければ既存 `session` ヘルパ（:392）に倣って badge/state 付きで作る。

- [ ] **Step 2: 失敗を確認する（RED）**

Run: `rtk cargo test outer_badge`
Expected: FAIL（`BadgeStyle::Outer` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装する（GREEN）**

`src/config/mod.rs` の enum に variant を追加:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BadgeStyle {
    #[default]
    Inline,
    Plain,
    /// グリフをセグメント塗りの外（バー地）に置く。docs/statusline-color-proposals.html の D3改。
    Outer,
}
```

`src/config/schema.rs:67` の enum を `["inline", "plain", "outer"]` に更新。

`render_session_segment`（:274-307）の先頭に Outer 分岐を追加:

```rust
    if badge_style == BadgeStyle::Outer {
        let body = style
            .format
            .replace("{badge}", "")
            .replace("{session}", &label)
            .replace("{index}", &(index + 1).to_string());
        let segment = tmux_style_segment(style, &body);
        if badge.is_empty() {
            return segment;
        }
        let glyph = match colors.for_state(state) {
            Some(color) => format!("#[fg={color}]{badge}#[default]"),
            None => badge.to_string(),
        };
        return format!("{glyph} {segment}");
    }
```

**設計意図:**
- グリフは `#[default]` で閉じてバー地の fg/bg に戻す。`badge_fragment` の「fg だけ復元」機構は Outer では使わない（塗りの外なので bg ごと戻す必要がある）。
- `{badge}` トークンは空置換（グリフは外に出るため format 内には現れない）。
- stale 時の `"?"`（state が `""`）は `for_state` が None を返すため無色 plain で外置きされる。
- 塗りの形状は矩形（キャップ glyph なし）。丸キャップは外置きグリフとゲシュタルト的に衝突するため使わない。`prefix`/`suffix` にキャップ文字を設定しないことは config 例（Task 6）で示す。

- [ ] **Step 4: テスト通過を確認する**

Run: `rtk cargo test --lib statusline`
Expected: PASS（新規2件を含む全件）

- [ ] **Step 5: コミット**

```bash
rtk git add src/config/mod.rs src/config/schema.rs src/statusline/mod.rs
rtk git commit -m "badge_style=outer でグリフを塗りの外に描画する"
```

---

## Task 4: summary を `badge.colors` に切り替える

**Files:**
- Modify: `src/daemon/mod.rs:102-124`（`render_summary`）、`:164`（呼び出し）
- Modify: `src/daemon/runtime.rs:717-725`（呼び出し）
- Test: `src/daemon/mod.rs:435-465` の既存3テスト

- [ ] **Step 1: 失敗するテストを書く**

`src/daemon/mod.rs:435` 付近の既存テストを新シグネチャ・新既定値に書き換える（これが RED になる）:

```rust
#[test]
fn render_summary_counts_states_with_markup_and_omits_zero() {
    // counts のセットアップは既存のまま
    let badge = crate::config::BadgeConfig::default();
    assert_eq!(
        render_summary(&counts, &badge),
        "#[fg=#ff6b6b]▲1#[default] #[fg=#4fd08a]●1#[default] #[fg=#45cbe6]✓1#[default] #[fg=#6f6b85]○1#[default]"
    );
}
```

※ 既存の期待値文字列（`fg=red` 等・idle 無色）を上記の形式（全状態 hex 色付き）に合わせて書き換える。`render_summary_is_empty_without_agents`（:451）と `fallback_summary_counts_idle_as_idle_not_done`（:458）も同様に第2引数を `&BadgeConfig::default()` にし、期待値に `#[fg=#6f6b85]` を反映する。

- [ ] **Step 2: 失敗を確認する（RED）**

Run: `rtk cargo test render_summary`
Expected: FAIL（シグネチャ不一致のコンパイルエラー）

- [ ] **Step 3: 実装する（GREEN）**

`render_summary`（:102-124）を書き換える:

```rust
pub fn render_summary(
    counts: &[(BadgeState, usize)],
    badge: &crate::config::BadgeConfig,
) -> String {
    counts
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(state, count)| {
            let glyph = glyph_for_state(*state, &badge.glyphs);
            let color = match state {
                BadgeState::Blocked => &badge.colors.blocked,
                BadgeState::Working => &badge.colors.working,
                BadgeState::Done => &badge.colors.done,
                BadgeState::Idle => &badge.colors.idle,
            };
            format!("#[fg={color}]{glyph}{count}#[default]")
        })
        .collect::<Vec<_>>()
        .join(" ")
}
```

呼び出し側2箇所を更新:
- `src/daemon/mod.rs:164`: `Ok(render_summary(&counts, &config.badge))`
- `src/daemon/runtime.rs:724`: `&self.config.badge.glyphs` → `&self.config.badge`

- [ ] **Step 4: テスト通過と全体確認**

Run: `rtk cargo test`
Expected: PASS（daemon・statusline・config 全件。ANSI 名前色のハードコードが残っていないことを `rtk proxy rg -n '"(red|green|cyan)"' src/` で確認 — attention の既定値（Task 5 で対応）以外にヒットが無いこと）

- [ ] **Step 5: コミット**

```bash
rtk git add src/daemon/mod.rs src/daemon/runtime.rs
rtk git commit -m "summary のグリフ色を badge.colors 参照に切り替える"
```

---

## Task 5: category ラベル展開と attention 既定色

**Files:**
- Modify: `src/config/mod.rs`（`StatuslineCategoryConfig` と `AttentionConfig` の Default）
- Modify: `src/statusline/mod.rs:137-182`（`render_statusline_category`）
- Test: `src/statusline/mod.rs` 内 `#[cfg(test)] mod tests`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn inactive_category_uses_inactive_format_and_name_token() {
    let mut config = Config::default();
    config.statusline.category.format = "{category} {name} ".to_string();
    config.statusline.category.inactive_format = "{category} ".to_string();
    config
        .categories
        .display_names
        .insert("work".to_string(), "🏠".to_string());
    config
        .categories
        .display_names
        .insert("net".to_string(), "🌐".to_string());
    let sessions = vec![session("main", "work"), session("web", "net")];
    let rendered = render_statusline_category(&config, &sessions, "work");
    // アクティブはアイコン＋名前、非アクティブはアイコンのみ
    assert!(rendered.contains("🏠 work "), "{rendered}");
    assert!(rendered.contains("🌐 "), "{rendered}");
    assert!(!rendered.contains("🌐 net"), "{rendered}");
}

#[test]
fn attention_segment_defaults_to_signal_red_hex() {
    let config = Config::default();
    let rendered = render_attention_segment(&config.statusline.attention, "▲ proxy · perm 2m");
    assert_eq!(rendered, "#[fg=#ff6b6b]▲ proxy · perm 2m#[default]");
}
```

※ 既存の `attention_segment_defaults_to_red_text`（:623）は上記に置き換える（`fg=red` → `fg=#ff6b6b`）。

- [ ] **Step 2: 失敗を確認する（RED）**

Run: `rtk cargo test --lib statusline`
Expected: FAIL（`inactive_format` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装する（GREEN）**

`StatuslineCategoryConfig` にフィールドを追加し、Default 実装に既定値を足す:

```rust
pub struct StatuslineCategoryConfig {
    // 既存フィールドはそのまま
    pub format: String,
    /// 非アクティブ category 用 format。既定はアクティブと同じ "{category} "。
    pub inactive_format: String,
    // ...
}

// impl Default for StatuslineCategoryConfig 内:
    inactive_format: "{category} ".to_string(),
```

`render_statusline_category`（:169-175）の body 組み立てを active で分岐させ、`{name}` トークン（raw な category 名）を追加:

```rust
            let format = if active {
                &config.statusline.category.format
            } else {
                &config.statusline.category.inactive_format
            };
            let body = format
                .replace("{category}", label)
                .replace("{name}", category)
                .replace("{count}", &category_sessions.len().to_string())
                .replace("{badge}", &badge);
```

`AttentionConfig` の Default で `fg: Some("red".to_string())` を `fg: Some("#ff6b6b".to_string())` に変更。

**幅ジッタの扱い:** ラベル展開で status-left の幅が category 切替のたびに変わる。Rust 側では対応せず、`status-left-length` の余裕確保を README スニペット（Task 6）で案内する。展開ラベルの truncate は今回のスコープ外（スコープ外の節を参照）。

- [ ] **Step 4: テスト通過を確認する**

Run: `rtk cargo test`
Expected: PASS 全件

- [ ] **Step 5: 品質ゲートを通してコミット**

```bash
rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test
rtk git add src/config/mod.rs src/statusline/mod.rs
rtk git commit -m "category の inactive_format と attention 既定色 hex 化を追加する"
```

---

## Task 6: README と roadmap の運用反映

**Files:**
- Modify: `README.md`（statusline 節、:101-102 の status-left/right 例の周辺）
- Modify: `docs/plans/2026-07-05-statusline-redesign-roadmap.md`（Plan 25 参照の1行追加）

- [ ] **Step 1: README に D3改 config 例を追記する**

statusline 節に以下をそのまま追加する（見出し名は既存構成に合わせる）:

````markdown
### D3改配色（推奨プリセット）

設計根拠と全案比較は `docs/statusline-color-proposals.html` を参照。
状態グリフは常にバー地の上に置き、塗りはカレント要素の名前だけに使う。

```yaml
# ~/.config/vde/tmux/config.yml
statusline:
  category:
    mode: list
    format: "{category} {name} "     # アクティブ: アイコン＋名前
    inactive_format: "{category} "   # 非アクティブ: アイコンのみ
    colors:                          # アクティブ = インディゴ矩形
      fg: "#ecebff"
      bg: "#453f9e"
    inactive_colors:
      fg: "#9591ad"

  sessions:
    badge_style: outer               # グリフを塗りの外（バー地）に出す
    current:
      format: " {session} "
      bold: true
      colors:
        fg: "#ecebff"
        bg: "#453f9e"
    other:
      format: " {session} "
      colors:
        fg: "#9591ad"

# badge.colors は既定で D3改の hex（変更する場合のみ記述）
# badge:
#   colors:
#     blocked: "#ff6b6b"
#     working: "#4fd08a"
#     done: "#45cbe6"
#     idle: "#6f6b85"
```

```tmux
# ~/.tmux.conf — バー地・window list は tmux 側の責務
set -ga terminal-overrides ',*:Tc'          # truecolor 必須（無いと hex が 256 色近似に落ちる）
set -g status-style 'bg=#1a1926,fg=#9591ad'
set -g status-left-length 60                # category ラベル展開の幅ジッタ対策
set -g window-status-format '#[fg=#9591ad] #I:#W '
set -g window-status-current-format '#[fg=#ecebff,bg=#453f9e] #I:#W '
set -g window-status-bell-style 'fg=#ff6b6b'
set -g window-status-activity-style 'fg=#ff6b6b'
```

注意:
- 塗りは矩形で使う。powerline キャップ（``等）を `prefix`/`suffix` に設定すると、外置きグリフとカプセル形状が視覚的に衝突する。
- breadcrumb 等でバーの下に別の面を重ねている場合、その地色を `#121218` 目安まで一段暗くしないとバー地 `#1a1926` と同化する。
````

- [ ] **Step 2: roadmap に参照を追加する**

`docs/plans/2026-07-05-statusline-redesign-roadmap.md` の plan 一覧（既存の書式に合わせる）に追加:

```markdown
- Plan 25: D3改配色（signal fg ＋ 矩形 accent-active） — `docs/plans/2026-07-06-plan-25-statusline-d3x-colors.md`
```

- [ ] **Step 3: tmux 実機 smoke**

tmux セッション内で実行し、出力に hex 色・outer グリフが含まれることを確認:

```bash
rtk proxy ./target/debug/vt statusline-sessions
rtk proxy ./target/debug/vt statusline-category
rtk proxy ./target/debug/vt statusline-summary
```

Expected: `#[fg=#4fd08a]●#[default]` 形式の出力（sessions）、`#[fg=#ff6b6b]▲1#[default]` 形式（summary）。確認結果（実行環境・気づき）を本 plan 末尾の「実装ノート」に記録する。

- [ ] **Step 4: コミット**

```bash
rtk git add README.md docs/plans/2026-07-05-statusline-redesign-roadmap.md docs/plans/2026-07-06-plan-25-statusline-d3x-colors.md docs/statusline-color-proposals.html
rtk git commit -m "D3改配色の設定例と比較資料を docs に追加する"
```

---

## スコープ外

- **window list / breadcrumb / バー地色の Rust 実装** — これらは tmux.conf の責務（Rust 側に描画コードが存在しない）。README スニペットでのみ扱う。
- **underline 属性の追加** — D3改はカレントを塗りで示すため不要（D1 を採用する場合のみ必要）。
- **展開ラベルの truncate（幅上限）** — `status-left-length` の案内で足りると判断。実運用で window list が押し出される事象が出たら別 plan で対応。
- **sidebar の配色** — sidebar は ratatui 側の独立した配色系（`sidebar.colors`）を持ち、本 plan の対象外。
- **category バッジ（`show_badge`）の outer 化** — category バッジは既定 off かつ D3改モックにも登場しないため、Inline のまま（色のみ Task 2 で hex 化）。
- **▲●✓○ のグリフ形状変更** — blocked(赤)/working(緑) の色覚対応は形状差が担保している。形状を変える変更はこの冗長性を壊すため本 plan では禁止。

## 実装ノート

- 2026-07-06 実装時点で `rtk cargo build` は成功した。
- tmux 実機 smoke は `./target/debug/vt statusline-sessions` / `statusline-category` / `statusline-summary` を実行し、いずれも exit 0。
  手元 config が `badge_style: outer` 未設定のため、sessions の実機出力は inline 形状のままだが、hex 色（例: `#[fg=#4fd08a]●#[fg=default]`）は確認した。
  outer 形状は `outer_badge_*` unit test で確認した。
- `statusline-summary` は既存 daemon socket が生きている場合、旧 daemon プロセス由来の `#[fg=green]` 応答を返した。
  ユーザー環境の daemon を停止せず、`VDE_DAEMON_SOCKET=/tmp/vde-tmux-codex-nonexistent.sock ./target/debug/vt statusline-summary` で fallback 経路を確認し、`#[fg=#4fd08a]●1#[default] #[fg=#6f6b85]○5#[default]` 形式を確認した。
  運用反映時は daemon 再起動後に summary の daemon 経路も新配色になる。
