# Sidebar 再設計ロードマップ(Plan 13〜17 統括)

サイドバー UI 再設計(F案 Command Center がゴール。背景と設計判断は `docs/sidebar-ui-proposals.md`、特に §9)を、一連の実装計画として一通で完了させるための統括ドキュメント。

## 実行順序と依存

| 順 | 計画書 | 内容 | 依存 |
|---|---|---|---|
| 1 | `2026-07-05-plan-13-sidebar-ui-phase1.md` | 表示基盤(グリフ・padding・右端カラム・truncate・フッター・RowMeta) | なし |
| 2 | `2026-07-05-plan-14-sidebar-ui-phase2.md` | inline meta・n/N 巡回・unread 即時既読・jump & return | Plan 13 |
| 3 | `2026-07-05-plan-15-sidebar-ui-phase3.md` | TRIAGE 常設ゾーン + FLEET・退出デバウンス | Plan 13, 14 |
| 4 | `2026-07-05-plan-16-sidebar-ui-phase4.md` | fisheye 完成(pin・3段階行高)・スクロール・幅ティア・rail 2部構成 | Plan 13〜15 |
| 5 | `2026-07-05-plan-17-sidebar-ui-phase5.md` | LIVE ペイン・イベントログ・フィルタバー多値化・フラッシュ・通知 | Plan 13〜16 |

**必ずこの順で、1 Plan ずつ完了させる。** 各 Plan 内も Task 順(TDD: 失敗テスト → 実装 → 全テスト → コミット)を守る。Plan を跨いだ先取り実装・Task の並行実施はしない。

## 各 Plan 共通のゲート(次の Plan に進む条件)

- [x] その Plan の DoD(機能/テスト/運用)が全項目チェック済み
- [x] `rtk cargo fmt --check` / `rtk cargo clippy --all-targets` / `rtk cargo test` が全通過
- [x] scratch tmux での smoke 実施と `docs/e2e-smoke.md` への記録(daemon 再起動を含む)
- [x] `docs/sidebar-ui-proposals.md` §9.2 の該当 Phase にチェック
- [x] コミット済み(Task 単位)。ワーキングツリーがクリーン

## 全体 DoD(ロードマップ完了条件)

### 機能完了条件

- [x] Plan 13〜17 の DoD がすべて満たされている
- [x] 幅40列で: フィルタバー付きヘッダー / TRIAGE(0件時消滅)/ FLEET ツリー / fisheye(選択フル・pin 中・他1行)/ LIVE / フッターが同時に成立する
- [x] 幅 30 / 24 / 8 / 2 列で dense / micro / rail に自動縮退し操作可能
- [x] `BadgeGlyphs` / `SidebarColorsConfig` の設定で絵文字・旧配色に戻せる

### テスト完了条件

- [x] `rtk cargo test` / `rtk cargo clippy --all-targets` / `rtk cargo fmt --check` 全通過

### 運用反映条件

- [x] `docs/e2e-smoke.md` が最終 UI に対応し、全 Plan の smoke 記録が残っている
- [x] README に focus バインド例・live/notify 設定例が記載されている
- [x] Phase 6 ゲート(下記)の判断材料が `docs/sidebar-ui-proposals.md` に追記されている

## 実装中の判断ルール

- 計画書とコードの実態が食い違う場合(行番号ズレ、既存リファクタとの衝突)は、**計画書の「意図」(DoD と表示フォーマット仕様)を正**とし、差分を該当 Plan の末尾に「実装ノート」として追記する。
- 計画書に無い設計判断が必要になったら、`docs/sidebar-ui-proposals.md` §9.1 の様式(採用/変更と根拠)で追記してから実装する。
- 後方互換対応・fallback は原則作らない(リポジトリ方針)。wire format 変更は daemon 再起動で吸収する。
- **採用しないと確定済みのもの**を実装しない: permission への send-keys 行内応答、ViewMode の完全廃止。

## Phase 6(再評価ゲート)— 実装しない。判断材料を集める

Plan 17 完了後、以下は**実装せず**、2週間程度の実運用を経てから判断する。判断材料として観察結果を `docs/sidebar-ui-proposals.md` に追記すること:

| 候補 | 採用判断の基準 |
|---|---|
| `▷` 最新レスポンス表示(hook/daemon 拡張) | LIVE ペインで「jump する価値の判断」が足りているか。足りているなら不採用 |
| worktree 連携(`w` spawn / `x` teardown) | vw(vde-worktree)の CLI 運用と比べてサイドバー起点の需要が実際にあるか |
| rich ティア(幅≥56 全行カード) | サイドバーを 56 列以上で使う運用が発生しているか |
| ViewMode 退役(自動密度への一本化) | v/1/2/3 を触る頻度。ほぼ 0 なら既定を隠しキーに降格 |
| スコープセレクタ(category→repo 絞り込み) | repo 数が増えて FLEET のスクロールが常態化しているか |
| pin の自動縮退(高さ不足時) | pin を4個以上使う運用が出ているか |

## Codex への引き渡しプロンプト(そのまま使用可)

```
docs/plans/2026-07-05-sidebar-redesign-roadmap.md を読み、記載の順序で
Plan 13 → 14 → 15 → 16 → 17 を実装してください。

ルール:
- 各 Plan の Task を順番に、TDD 手順(失敗テスト → 実装 → 全テスト通過 → コミット)で進める
- 各 Plan のゲート(DoD 全チェック + fmt/clippy/test + smoke 記録)を満たすまで次の Plan に進まない
- smoke は scratch tmux で実施し、daemon 再起動を忘れない
- 計画書と実コードが食い違ったら DoD と表示フォーマット仕様を正とし、差分を Plan 末尾に実装ノートとして追記する
- Phase 6 の項目(ロードマップ参照)は実装しない
- 背景設計は docs/sidebar-ui-proposals.md(特に §9)を参照する
```
