# Plan 24: 展開ビューの情報再構成と色規約

> **実装者向け:** `docs/sidebar-detail-view-proposals.html` の推奨案(5A / 1A / 2A+3A / tasks)の実装。**Plan 13〜23 完了が前提**。Task 順(独立・依存順)に実施する。各 Task は TDD で進め、Task ごとにコミットする。

**Goal:** fisheye 展開時の情報重複(prompt 二重・経過時間二重)を解消し、状態行を状態色で装飾、メタ 3 行を「状態行+場所行」の 2 行に統合、時間表記を humanize し、branch 色の衝突を解消して 5 族の色規約を明文化する。

**Architecture:** すべて `tree.rs`(行構築)と `render.rs`(描画)で完結する。completed_at は `PaneSnapshot` に既存のため daemon の新規ポーリングは不要(`AgentPane` への配線のみ)。状態行のグリフと色は render 側の既存 badge 機構(`row.badge_state` + theme)を Detail 行へ拡張して実現し、daemon から theme 情報を渡す必要はない。

**Tech Stack:** Plan 13〜23 と同じ(新規依存なし)

## DoD

### 機能完了条件

- [x] git branch 名の既定色が淡シアン(Indexed 73)になり、✓ done(明シアン)との衝突が解消される。`colors.branch` で従来色に戻せる
- [x] 展開中の chat 行が agent 名のみ(例 `▾ ○ claude`)になり、右ラベル(経過時間等)も非表示になる。折りたたみ時は従来どおり
- [x] 展開の status/elapsed/session 3 行が「状態行+場所行」の 2 行になる:
  - 状態行: `● running · 12m` / `○ idle · done 38h ago` / `▲ permission (approve request?) · 2m` — グリフと状態語のみ状態色、残りは detail 色
  - 場所行: `vde-tmux · %51`(detail 色)
- [x] 時間の意味が状態で切り替わる: blocked/working=started_at からの経過、idle(done 含む)=completed_at からの `done {t} ago` 表記(completed_at 不明なら時間部を省略)。**表記は英語で統一する(日本語の「〜前に完了」は使わない)**
- [x] 経過時間表記が全箇所(状態行・chat 右ラベル・pin 要約)で humanize される: `45s` / `12m` / `1h30m` / `38h` / `2d`
- [x] running 中は状態行末尾に `· 3/5 tasks`(tasks_total > 0 のときのみ)が付く
- [x] README(または theme コメント)に 5 族の色規約(状態/構造/操作/本文/実況)が記載される

### テスト完了条件

- [x] `rtk cargo test` 全通過(status:/elapsed:/session: 前提の既存テストの期待値更新を含む)
- [x] 新規テスト: humanize の境界値、展開中 chat 行の縮約と右ラベル抑制、状態行の状態別ラベル生成(running/idle/blocked)、completed_at 欠落時の省略、状態行の span 色分け、場所行、tasks 表示条件、branch 既定色
- [x] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

### 運用反映条件

- [x] `docs/e2e-smoke.md` に展開ビュー新形式の確認手順と smoke 実施結果を追記
- [x] README に色規約と展開ビューの表示仕様を追記
- [x] `docs/sidebar-detail-view-proposals.html` の推奨案がすべて実装されたことを本計画書のチェックで担保

---

## Task 0: branch 既定色の変更と色規約の明文化(5A)

**Files:**
- Modify: `src/sidebar/render.rs`(`SidebarRenderTheme` の Default と struct コメント)
- Modify: `README.md`

- [x] **Step 1: 失敗するテストを書く**

`src/sidebar/render.rs` tests:

```rust
#[test]
fn branch_defaults_to_muted_cyan() {
    assert_eq!(SidebarRenderTheme::default().branch, Color::Indexed(73));
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `rtk cargo test --lib sidebar::render::tests::branch_defaults_to_muted_cyan`
Expected: FAIL(現在は `Color::Cyan`)

- [x] **Step 3: 実装**

1. `Default` の `branch: Color::Cyan` を `branch: Color::Indexed(73)` に変更。
2. `SidebarRenderTheme` の struct 冒頭に色規約コメントを追加:

```rust
/// サイドバーの配色。色は 5 族の規約で運用する:
/// - 状態族: badge_* / rollup 色(▲赤 ●緑 ✓シアン ○灰)。状態を示す場所にだけ使う
/// - 構造族: repo(青太字)/ category(ピーチ太字)/ branch(淡シアン 73)
/// - 操作族: ラベンダー 147/103(pin ✦ / mode ≣ / active ▎ / preview ⌕)
/// - 本文族: 本文=通常色 / 補足=detail(246)/ 記号=marker(暗灰)
/// - 実況: live(マゼンタ)は LIVE/EVENTS 見出し専用の孤立色
```

3. README の sidebar colors 説明に同じ 5 族規約を 1 段落で追記し、`branch` の既定値変更(Cyan → 73)を明記。

- [x] **Step 4: テスト通過を確認してコミット**

Run: `rtk cargo test`(branch 色を参照する既存テストがあれば期待値を 73 に更新)

```bash
rtk git add -A
rtk git commit -m "branch 既定色を淡シアンにして色規約を明文化する"
```

---

## Task 1: 経過時間の humanize 共通化

**Files:**
- Modify: `src/sidebar/tree.rs`(`humanize_secs` 新設、`meta_label` の経過表記置換)
- Modify: `src/sidebar/render.rs`(`elapsed_label` 921-927行 を委譲)

- [x] **Step 1: 失敗するテストを書く**

`src/sidebar/tree.rs` tests:

```rust
#[test]
fn humanize_secs_formats_by_magnitude() {
    assert_eq!(humanize_secs(0), "0s");
    assert_eq!(humanize_secs(45), "45s");
    assert_eq!(humanize_secs(60), "1m");
    assert_eq!(humanize_secs(12 * 60 + 30), "12m");
    assert_eq!(humanize_secs(90 * 60), "1h30m");
    assert_eq!(humanize_secs(10 * 3600), "10h");
    assert_eq!(humanize_secs(38 * 3600 + 59 * 60), "38h");
    assert_eq!(humanize_secs(48 * 3600), "2d");
    assert_eq!(humanize_secs(100 * 3600), "4d");
    assert_eq!(humanize_secs(-5), "0s");
}
```

- [x] **Step 2: テストが失敗することを確認 → 実装**

`src/sidebar/tree.rs`:

```rust
/// 経過秒の人間可読表記。<60s は秒、<60m は分、<10h は時+分、
/// <48h は時のみ、以上は日のみ(桁が大きいほど粒度を落とす)。
pub fn humanize_secs(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 10 {
        let rest = minutes % 60;
        if rest == 0 {
            return format!("{hours}h");
        }
        return format!("{hours}h{rest:02}m");
    }
    if hours < 48 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}
```

`src/sidebar/render.rs` の `elapsed_label`(921-927行)を委譲に置換:

```rust
fn elapsed_label(secs: i64) -> String {
    crate::sidebar::tree::humanize_secs(secs)
}
```

`src/sidebar/tree.rs` の `meta_label`(pin 要約 1 行)内で経過秒を `{}m` / `{}s` 形式にしている箇所を `humanize_secs` 呼び出しへ置換(該当箇所は `meta_label` 内の elapsed 系 format。実装時に確認して同関数内をすべて揃える)。

既存テストで `"976m"` 等の分表記を期待しているものは humanize 後の値(`16h`)へ更新する。

- [x] **Step 3: テスト通過を確認してコミット**

Run: `rtk cargo test`

```bash
rtk git add -A
rtk git commit -m "経過時間表記を humanize_secs に共通化する"
```

---

## Task 2: 展開中 chat 行の縮約(1A)

**Files:**
- Modify: `src/sidebar/tree.rs`(`push_chat_row` 427-459行、TRIAGE 内 chat 行 382-411行)
- Modify: `src/sidebar/render.rs`(`right_label` 898-918行)

- [x] **Step 1: 失敗するテストを書く**

`src/sidebar/tree.rs` tests:

```rust
#[test]
fn expanded_chat_row_shows_agent_name_only() {
    // 選択(=展開)された chat 行の label が agent 名のみになる。
    // 折りたたみ時は従来の chat_label(prompt 入り)のまま。
    // TRIAGE 内の展開 chat 行も同様に agent 名のみ。
}
```

`src/sidebar/render.rs` tests:

```rust
#[test]
fn expanded_chat_row_suppresses_right_label() {
    // kind=Chat, expanded=true, rollup=Running(elapsed あり)の行で
    // right_label(&row) == None。expanded=false なら Some("12m")
}
```

- [x] **Step 2: テストが失敗することを確認 → 実装**

`src/sidebar/tree.rs` の `push_chat_row`(441-453行の label 部分):

```rust
    let label = if expanded {
        pane.agent.clone()
    } else {
        chat_label(pane)
    };
```

TRIAGE 内 chat 行(`triage_zone_rows`、392-404行の `label: format!("{} · {}", pane.agent, pane.repo)`)も同様に:

```rust
    let label = if expanded {
        pane.agent.clone()
    } else {
        format!("{} · {}", pane.agent, pane.repo)
    };
```

`src/sidebar/render.rs` の `right_label` の Chat 分岐冒頭に追加:

```rust
        SidebarRowKind::Chat => {
            if row.expanded {
                // 展開中は経過時間等をメタ行(状態行)に一本化する
                return None;
            }
            ...既存の match row.rollup...
        }
```

- [x] **Step 3: テスト通過を確認してコミット**

Run: `rtk cargo test`(展開行のラベルを期待している既存テストを更新)

```bash
rtk git add -A
rtk git commit -m "展開中の chat 行を agent 名のみに縮約する"
```

---

## Task 3: メタ行の再構成 — 状態行+場所行(2A+3A)

**Files:**
- Modify: `src/sidebar/tree.rs`(`AgentPane`、構築箇所 171-190行、`push_chat_detail_rows` 513-566行)
- Modify: `src/sidebar/render.rs`(badge 表示条件、状態行の span 分割)

- [x] **Step 1: 失敗するテストを書く**

`src/sidebar/tree.rs` tests:

```rust
#[test]
fn detail_rows_are_state_and_place_lines() {
    // running の pane(started_at = now-720)を展開:
    // - detail::%1::state の label == "running · 12m"
    // - detail::%1::place の label == "{session} · %1"
    // - detail::%1::status / ::elapsed / ::session 行は存在しない
}

#[test]
fn idle_state_line_uses_completed_at() {
    // idle の pane(completed_at = now-136800 [38h前])→ "idle · done 38h ago"
    // completed_at が空 → "idle"(時間部なし)
}

#[test]
fn blocked_state_line_keeps_wait_reason() {
    // status=waiting, wait_reason=permission_prompt, started_at=now-120
    // → "waiting (permission_prompt) · 2m" 相当
    //(状態語は既存 status_label の出力に従う)
}

#[test]
fn running_state_line_appends_tasks_progress() {
    // tasks="3/5" 相当のデータ → "running · 12m · 3/5 tasks"
    // tasks_total == 0 または非 running では付かない
}
```

`src/sidebar/render.rs` tests:

```rust
#[test]
fn state_detail_row_colors_glyph_and_state_word() {
    // kind=Detail, id="detail::%1::state", badge_state=Some(Working),
    // label="running · 12m" を幅40で描画:
    // - "● " span と "running" span が theme.badge_working 色
    // - " · 12m" span が theme.detail 色
    // place 行(id=...::place)は従来どおり全体 detail 色
}
```

- [x] **Step 2: テストが失敗することを確認 → 実装**

`src/sidebar/tree.rs`:

1. `AgentPane` に `completed_at: String` を追加し、構築箇所(171-190行の `AgentPane {`)で `completed_at: pane.completed_at.clone(),` を設定(`PaneSnapshot.completed_at` は既存)。
2. `push_chat_detail_rows` の status/elapsed/session 3 行(517-536行)を差し替え:

```rust
    // 状態行: "{状態語}[ (wait_reason)] · {状態依存の時間}[ · {done}/{total} tasks]"
    // グリフは render 側が badge_state から付けるため label には含めない
    let mut state = status_label(&pane.status).to_string();
    if let Some(wait_reason) = non_empty(&pane.wait_reason) {
        state.push_str(&format!(" ({wait_reason})"));
    }
    if let Some(time) = state_time_label(pane, now) {
        state.push_str(&format!(" · {time}"));
    }
    if pane.badge_state == BadgeState::Working
        && let Some((done, total)) = parse_tasks(&pane.tasks).filter(|(_, total)| *total > 0)
    {
        state.push_str(&format!(" · {done}/{total} tasks"));
    }
    rows.push(detail_row(pane, depth, "state", state));
    rows.push(detail_row(
        pane,
        depth,
        "place",
        format!("{} · {}", pane.session, pane.pane_id),
    ));
```

3. 状態依存の時間ヘルパを新設:

```rust
/// 状態行の時間部。blocked/working は started_at からの経過、
/// idle(done 含む)は completed_at からの `done {t} ago`(英語表記)。不明なら None。
fn state_time_label(pane: &AgentPane, now: i64) -> Option<String> {
    match pane.badge_state {
        BadgeState::Blocked | BadgeState::Working => {
            let started_at = pane.started_at.parse::<i64>().ok()?;
            Some(humanize_secs(now - started_at))
        }
        BadgeState::Done | BadgeState::Idle => {
            let completed_at = pane.completed_at.parse::<i64>().ok()?;
            Some(format!("done {} ago", humanize_secs(now - completed_at)))
        }
    }
}
```

(`parse_tasks` は既存関数。tuple の型が合わない場合は既存実装に合わせて調整。)

`src/sidebar/render.rs`:

4. badge(グリフ)の表示条件(552行付近)に state 行を追加:

```rust
    let is_state_detail =
        row.kind == SidebarRowKind::Detail && row.id.ends_with("::state");
    let badge = if row.kind == SidebarRowKind::Chat || is_state_detail {
        row.badge_state.map(|state| { ... 既存のまま ... })
    } else {
        None
    };
```

(グリフの幅は既存の badge_width 計算にそのまま乗る。)

5. `label_spans` 呼び出し部で state 行の色分けを追加(`spans.extend(label_spans(...))` の分岐に併設):

```rust
    if is_state_detail {
        // 状態語(最初のトークン)のみ状態色、残りは detail 色
        let color = row
            .badge_state
            .map(|state| theme.badge_color(state))
            .unwrap_or(theme.detail);
        let (word, rest) = match label.split_once(' ') {
            Some((word, rest)) => (word.to_string(), format!(" {rest}")),
            None => (label.clone(), String::new()),
        };
        spans.push(Span::styled(word, Style::default().fg(color)));
        if !rest.is_empty() {
            spans.push(Span::styled(rest, Style::default().fg(theme.detail)));
        }
    } else if row.kind == SidebarRowKind::Jump {
        ... 既存 ...
```

- [x] **Step 3: テスト通過を確認してコミット**

Run: `rtk cargo test`(`status:` / `elapsed:` / `session:` を期待する既存テスト — tree の行構成テスト・render の描画テスト・`docs` 内の期待値 — をすべて新形式へ更新)

```bash
rtk git add -A
rtk git commit -m "展開メタを状態行と場所行の2行に再構成する"
```

---

## Task 4: 品質ゲート・smoke・ドキュメント

- [x] **Step 1: 品質ゲート**

Run: `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test`
Expected: すべて通過

- [x] **Step 2: バイナリ反映と smoke**

```bash
cargo install --path . --force
```

scratch tmux で確認(サイドバーは `M-e` ×2 で再起動):

- branch 名が淡シアンになり、✓ done のシアンと見分けられる
- chat 行を選択(展開)すると agent 名のみ+右ラベルなしになり、折りたたむと従来表示に戻る
- 展開が「prompt 全文 / 状態行 / 場所行 / (subagents) / アクション行」の構成になっている
- idle の agent の状態行が `○ idle · done Nh ago`(completed_at 不明の pane では `○ idle`)
- running の agent で `● running · 12m · 3/5 tasks`(タスクなしなら tasks 部なし)
- permission 待ちで `▲ waiting (permission_prompt) · 2m` 相当が赤系で表示される
- 折りたたみ行の右ラベルが `16h` / `2d` 等の humanize 表記になっている
- pin の要約 1 行の経過表記も humanize されている

結果を `docs/e2e-smoke.md` に追記。

- [x] **Step 3: docs 更新とコミット**

- README: 展開ビューの表示仕様(状態行・場所行・時間の意味)と 5 族の色規約を追記(Task 0 で入れた分と重複しないよう統合)
- 本計画書の DoD チェックを更新し、「実装ノート」に計画からの差分を記載

```bash
rtk git add -A
rtk git commit -m "Plan 24 の smoke 結果と docs を更新する"
```

## スコープ外

- EVENTS フィードの時刻表記(`{n}m前`)の humanize 統一 — 表示幅が異なるため別判断
- 場所行の「session 名が repo 行と同じ場合は省略」最適化 — 初期実装では常時表示
- 最新レスポンス 1 行(▷)の表示 — LIVE と役割重複のため見送り(提案書 Item 4 の判定どおり)
- Dense / Micro ティアの表示変更(展開ビューは Standard 専用のため対象外)

## 実装ノート

- Task 4 のバイナリ反映は、計画書内の例では `cargo install --path . --force` だったが、実行コマンド規約に合わせて `rtk cargo install --path . --force` で実施した。
- TUI の alt-screen capture は既存 smoke と同様に安定しないため、Plan 24 の表示仕様 smoke は `rtk cargo fmt --check && rtk cargo clippy --all-targets && rtk cargo test` と、対応する tree/render/runtime 回帰テストで担保した。結果は `docs/e2e-smoke.md` に追記した。
- blocked/permission 待ちの状態語は計画書 Step 2 の注記どおり既存 `status_label` に従い、`waiting (permission_prompt) · 2m` とした。
- idle/done の完了時刻表記はすべて英語の `done {t} ago` に統一し、日本語表記は追加していない。
