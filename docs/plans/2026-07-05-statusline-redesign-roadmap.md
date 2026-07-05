# Statusline 再設計ロードマップ(Plan 19〜22 統括)

tmux ステータスライン再設計(背景と設計判断は `docs/statusline-ui-proposals.md`、特に §7)を一通で完了させるための統括ドキュメント。sidebar 再設計(Plan 13〜17 + Plan 18 の修正)の後続として実施する。

## 前提条件(着手前に確認)

- [ ] sidebar Plan 13〜17 が実装済み(単幅グリフ ▲●✓○、runtime の triage 集合・unread 管理が存在する)
- [ ] **Plan 18(sidebar レビュー指摘の修正)が完了している**。未完了なら本ロードマップに着手しない
- [ ] `rtk cargo test` が green の状態から始める

## 実行順序と依存

| 順 | 計画書 | 内容 | 依存 |
|---|---|---|---|
| 1 | `2026-07-05-plan-19-statusline-phase1.md` | 表示基盤(suffix 既定・hide_idle・current 既定 bold) | Plan 13〜18 |
| 2 | `2026-07-05-plan-20-statusline-phase2.md` | `statusline-summary` への置換(`running:N` 廃止・dead config 整理) | Plan 19 |
| 3 | `2026-07-05-plan-21-statusline-phase3.md` | 案A-lite(`@vde_session_state` 配信・badge_style inline・category {count}) | Plan 19, 20 |
| 4 | `2026-07-05-plan-22-statusline-phase4.md` | `statusline-attention`・heartbeat / stale・README 文書化 | Plan 19〜21 + sidebar Plan 15 |

**必ずこの順で、1 Plan ずつ完了させる。** 各 Plan 内も Task 順(TDD: 失敗テスト → 実装 → 全テスト → コミット)を守る。Plan を跨いだ先取り実装はしない。

## 各 Plan 共通のゲート(次の Plan に進む条件)

- [ ] その Plan の DoD(機能/テスト/運用)が全項目チェック済み
- [ ] `rtk cargo fmt --check` / `rtk cargo clippy --all-targets` / `rtk cargo test` が全通過
- [ ] scratch tmux での smoke 実施と `docs/e2e-smoke.md` への記録(daemon 再起動を含む)
- [ ] `docs/statusline-ui-proposals.md` §7.2 の該当 Step にチェック
- [ ] コミット済み(Task 単位)。ワーキングツリーがクリーン

## 全体 DoD(ロードマップ完了条件)

### 機能完了条件

- [ ] Plan 19〜22 の DoD がすべて満たされている
- [ ] 実運用相当の tmux.conf(pill 装飾 + `#()` 呼び出し)で: グリフ付きセッション列挙 / current 強調 / summary / attention / stale 表示が同時に成立する
- [ ] daemon 停止時に sessions/category は継続動作し、バッジは `?` に落ち、summary/attention はフォールバックで動く

### テスト完了条件

- [ ] `rtk cargo test` / `rtk cargo clippy --all-targets` / `rtk cargo fmt --check` 全通過

### 運用反映条件

- [ ] `docs/e2e-smoke.md` が最終仕様に対応し、全 Plan の smoke 記録が残っている
- [ ] README に status-interval 推奨・status-left/right 設定例・新 config(badge_style / hide_idle / {count} / summary / attention)が記載されている
- [ ] `docs/migration.md` の M7(dotfiles 切替)手順に `statusline-agent-badge` → `statusline-summary` と suffix 既定変更が反映されている

## 実装中の判断ルール

- 計画書とコードの実態が食い違う場合は、**計画書の「意図」(DoD)を正**とし、差分を該当 Plan 末尾に「実装ノート」として追記する
- 計画に無い設計判断が必要になったら `docs/statusline-ui-proposals.md` §7.1 の様式で追記してから実装する
- 後方互換対応・fallback は原則作らない(リポジトリ方針)。コマンド削除・config キー変更は migration.md への記載で吸収する
- **実装しないもの(Step 5 の再評価ゲート項目)**: category `{badge}`(attention の下位互換になるため不要判定が濃厚。ただし Plan 21 の `@vde_session_state` で実装コストは下がっている)、案B `state_colors`(pill 背景の状態色化)、案D current-only モード、option bus 移行、クリック対応(`--click`)。`SegmentColors.outer_bg` の削除も Step 5 判断
- sidebar 側のコード(render.rs / tree.rs / tui.rs)には触れない。共有部(config の badge.glyphs、daemon の runtime/protocol)の変更は各 Plan の指定範囲のみ

## Codex への引き渡しプロンプト(そのまま使用可)

```
docs/plans/2026-07-05-statusline-redesign-roadmap.md を読み、前提条件を確認した上で、
記載の順序で Plan 19 → 20 → 21 → 22 を実装してください。

ルール:
- 各 Plan の Task を順番に、TDD 手順(失敗テスト → 実装 → 全テスト通過 → コミット)で進める
- 各 Plan のゲート(DoD 全チェック + fmt/clippy/test + smoke 記録)を満たすまで次の Plan に進まない
- smoke は scratch tmux で実施し、daemon 再起動を忘れない
- 計画書と実コードが食い違ったら DoD を正とし、差分を Plan 末尾に実装ノートとして追記する
- ロードマップの「実装しないもの」(Step 5 項目)は実装しない
- 背景設計は docs/statusline-ui-proposals.md(特に §7)を参照する
- 前提条件(sidebar Plan 13〜18 完了、cargo test green)が満たされていない場合は
  実装を開始せず、その旨を報告して停止する
```
