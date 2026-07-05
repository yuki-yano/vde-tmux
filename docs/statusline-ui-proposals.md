# vde-tmux statusline 表示形式の再設計案(評価用ドキュメント)

- 基準コミット: f7587a3
- 作成: Claude Code(statusline 実装調査 + 実運用スクリーンショットの分析に基づく)
- 前提: サイドバー再設計(`docs/sidebar-ui-proposals.md` §9、Plan 13〜17)と状態語彙を共有する
- HTML 版(pill モック色付き): `docs/statusline-ui-proposals.html`

## 0. ステータスバーの役割定義(主張)

| | サイドバー(Plan 13〜17) | ステータスバー |
|---|---|---|
| 性格 | triage と操作の場。開いているときだけ存在 | 常時視界にある1行。操作は最小 |
| 答える問い | どれから対応するか / どこまで進んだか | **今いる場所はどこか** / **視界の外で何かが自分を待っていないか** |
| 情報の深さ | agent 単位、prompt・進捗・LIVE まで | 集約値と例外のみ |

本質は「現在地の表示」と「不在検知の安全網」の2つ。それ以外はノイズ候補。

## 1. 現状(調査結果の要約)

### 実装

- `vt statusline-sessions [--show-index]`: 現 category の session 列挙。session badge(`@vde_session_status`、daemon が SetSessionBadge effect で書く)を `{badge}{label}` で前置。daemon 非依存(list-sessions のみ)
- `vt statusline-category`: category 名の列挙のみ(件数なし)。active/inactive 色は config 未設定なら無差別
- `vt statusline-agent-badge`: `running:2` 形式(rollup英語ラベル:全pane数)。daemon Query + list-panes フォールバックの2段構え
- tmux.conf 側で `#()` コマンド置換(status-interval ごとに fork)。切替は `statusline-sessions switch <n>` / `statusline-category switch <n>`(1-origin)

### 実運用の見た目(vde-tmux-manager + dotfiles、スクリーンショットより)

```
[🏠] [🌐]  1 duel-logger  2 opener-rate  3 vde-monitor  [4 vde-tmux]   …   [1 node]
```

- カテゴリ = アイコン pill(`display_names`)。アクティブはオレンジ
- セッション = index + 名前。current はラベンダーの塗り pill
- 右端 = window pill。pill 装飾は dotfiles(prefix/suffix/colors)の管轄

### 課題(調査で確認)

1. 情報の重複: session badge(🔴)と `running:2` が同じ情報を別語彙で二重表示
2. category に件数がない・active 色の既定なし
3. current session のスタイル既定が other と同一(無区別)
4. 絵文字幅問題を `session_badge.suffix: " "` で対症療法(plan-09 に明記)
5. dead config: `statusline.agent_badge.enabled` は未配線(README には記載)。`SegmentColors.outer_bg` も未使用
6. daemon クラッシュ時に `@vde_session_status` が stale のまま残る(偽の健全表示)
7. 更新遅延(daemon poll_ms + status-interval)の推奨値が未文書化
8. クリック対応(`#[range=...]`)は非生成でユーザー自作頼み

## 2. 共通前提(全案)

- **pill の骨格は壊さない**: カテゴリ=アイコン pill / セッション=index+名前 / current=塗り pill / window=右端 pill。提案が加えるのは状態(▲●✓○)のレイヤーだけ。pill 装飾は dotfiles 管轄のまま
- **グリフと色の統一**: ▲(blocked・赤)●(working・緑)✓(done・シアン)○(idle・無彩)。`badge.glyphs` は sidebar と共有 top-level 設定のため、**Plan 13 の既定値変更で自動的に単幅グリフ化されるのは session badge のみ**。`statusline-agent-badge`(`running:N`)は summary 置換という明示作業、`session_badge.suffix: " "` の既定も明示的な変更が必要(自動では変わらない)
- **件数表記の統一**: `▲2 ●1` 形式(0件省略)。`running:2` 表記は廃止し `vt statusline summary` に置換。dead config の `agent_badge.enabled` はこのとき配線
- **current の既定スタイル**: 未設定でも bold+色が付く既定へ

### サイドバーとの併用整合(4ルール)

1. 状態4色(赤・緑・シアン・無彩)は状態専用に予約。装飾 pill は被らない色相(ラベンダー・オレンジ・クリーム等)
2. 塗り背景の意味は「現在地/選択」と「警報(赤のみ)」の2つに限定(バーの current pill ↔ サイドバー選択行 bg、赤 pill ↔ TRIAGE 赤見出し)
3. グリフは両面で同一セット・同一色
4. **状態グリフは pill と一体化させ、浮かせない**。裸のグリフを pill の外に置くと所属が曖昧で pill 列から浮く。かといって状態色を装飾背景に直置きするとコントラストが崩れる。解は2つ:
   - (a) **内側 + 色バリアント(推奨・既定)**: pill 内にグリフを置き、背景明度に応じた濃色/明色バリアントを使う(`[▲ 4 vde-tmux]`、ラベンダー上は深紅)。pill の一体感を保ち、実装も最軽量
   - (b) **分割コンパートメント(opt-in)**: pill 左端にバッジ専用の状態色区画を融合(`[▲|4 vde-tmux]` — 赤地白▲ + ラベンダー本体)。powerline 的でより主張が強い
   - 非 pill 要素(通常セッションの `●2 opener-rate`)は従来どおり裸のグリフ前置(地の上なら浮かない)
   - 実装: renderer が badge 部分に個別スタイルを出す `badge_style: inline | compartment | plain` の新設が必要(既定 inline。inline は `#[fg=<state_variant>]▲#[fg=<label_fg>]` で bg は pill のまま)。現行は `{badge}{label}` が segment body 内で単色展開される

## 3. 提案(A〜D)

### 案A: Glyph Injection — 現行 pill に状態グリフを注入

```
[🏠] [▲ 🌐]  ○1 duel-logger  ●2 opener-rate  ✓3 vde-monitor  [▲ 4 vde-tmux]   …   ▲1 ●1 ✓1 ○1  [1 node]
```

(`[▲ ...]` は pill 内側バッジ: 明るい pill 背景に濃色バリアントの状態グリフ)

- セッションへのグリフ前置は実装済み機構(session badge)。Plan 13 で 🔴→▲ に自動化。**idle(○)も既定で表示** — 無印の意味が「agent なし」だけに純化され、「idle の agent が居る」と「agent が居ない」が区別できる。静かにしたい人向けに `hide_idle_badge: true` を用意(既定 false)
- pill 付き要素(current session・カテゴリ)のバッジは**内側配置 + 背景対応の色バリアント**(併用整合ルール4)。`badge_style: inline | compartment | plain` の新設が前提(既定 inline)。非 pill セッションは裸のグリフ前置のまま
- category format に `{badge}` 追加(カテゴリ所属 session の最悪状態を注入、新実装)。非アクティブカテゴリにも ▲ が付けば「別カテゴリで待たれている」がカテゴリ粒度で分かる
- 右端に summary(`▲1 ●1`)
- Pros: dotfiles 無変更で移行 / Cons: 「どの session か」は現 category 外だと不明。規模 中(badge_style・summary・dead config 配線を含む。category `{badge}` はグリフ→BadgeState 逆引き or 構造化経路が必要で 中〜大 のため §7 で後送り)

### 案B: State Pills — pill の背景色 = 状態

```
[🏠] [🌐]  [1 duel-logger](暗) [2 opener-rate](緑) [3 vde-monitor](シアン) [4 vde-tmux](赤)   …   [1 node]
```

- session pill の bg を badge 状態に連動。current は同色系高明度+太字。幅コストゼロで判別最速
- 要新設: `sessions.state_colors.{blocked,working,done,idle}` とスタイル解決の条件分岐
- Cons: 「塗り=現在地」の文法が崩れ装飾 pill と意味衝突(併用整合ルール2に抵触)/ 色覚特性をグリフなしで受ける / 改修最大。規模 中。**opt-in 扱いを推奨**

### 案C: Exception Channel — 右端に「よそで待たれているもの」だけ

```
(左は現行のまま)   …   [▲ llm-proxy · perm 2m] ●2  [1 node]     ← blocked あり
(左は現行のまま)   …   ●3 ✓1  [1 node]                          ← 平常時は集約のみ
```

- `vt statusline attention` 新設: 現在 attach していない session の blocked を `▲ {session} · {reason} {経過}` で名指し(最古1件 + `+N`)、残りは集約
- 「見えている場所の問題は報せない。見えていない場所の問題だけ報せる」— サイドバー TRIAGE と対
- データは daemon Query + list-panes フォールバック(agent-badge と同じ2段構え)
- Cons: 右端幅が動的(固定幅確保オプションで緩和)/ daemon 停止時は沈黙(heartbeat 前提)。規模 小〜中

### 案D: Category Dashboard — アイコン pill を集約器に

```
[🏠 2] [▲ 🌐 4]  [▲ 4 vde-tmux]   …   ▲1 ●2  [1 node]
```

- セッション列挙を畳み、カテゴリ pill = 内側バッジ(最悪状態)+ アイコン + 件数。current session pill のみ残す
- Cons: index 即切替の筋肉記憶を壊す(最大の代償)。session 数が列挙に耐えなくなったときの縮退先として config 選択制。規模 小〜中

## 4. 運用アーキテクチャ

| | 現行: `#()` コマンド置換 | 提案: option bus 配信 |
|---|---|---|
| 仕組み | status-interval ごとに vt を fork | daemon が描画済み文字列を `@vde_statusline_left/right` に書き、tmux.conf は option 参照 |
| 更新コスト | 3 fork/interval | 差分時のみ set-option、fork ゼロ |
| daemon 停止時 | sessions/category は素で動く(badge のみ stale) | 文字列全体が stale — heartbeat 必須 |

- **stale 対策(どちらでも導入)**: daemon が `@vde_heartbeat`(epoch)を書き、5秒超過でバッジ・サマリを dim `?` に落とす
- 推奨: 当面 `#()` 維持、Plan 17 完了後に option bus を再評価。`status-interval 1` 推奨を README に明記

## 5. 表示案と独立の修正・機能(優先度順)

| 優先 | 項目 |
|---|---|
| 高 | グリフ語彙統一(Plan 13 相乗り)。suffix 既定の再確認 |
| 高 | dead config 整理(`agent_badge.enabled` 配線。`outer_bg` は削除に傾く — `SegmentColors` に deny_unknown_fields は無く削除しても既存 config はエラーにならない(config/mod.rs:87-93)。pill 接続機能の予定が立たない限り dead field は消す) |
| 高 | current 既定スタイル |
| 中 | category `{count}` / `{badge}` プレースホルダ |
| 中 | `badge_style: inline / compartment / plain`(pill 付き要素でのバッジ描画方式、既定 inline。案A採用時は必須) |
| 中 | heartbeat / stale 表示 |
| 中 | status-interval 推奨の文書化 |
| 低 | クリック対応(`--click` で `#[range=user|...]` 生成) |
| 低 | window-status バッジ(plan-09 の non-goal 継続を推奨) |

## 6. 推奨

**案A + 案C を既定。案B は `state_colors` の opt-in。案D は縮退オプション。**

推奨形:

```
[🏠] [▲ 🌐]  ○1 duel-logger  ●2 opener-rate  ✓3 vde-monitor  [▲ 4 vde-tmux]   …   [▲ llm-proxy · perm 2m] ●2  [1 node]
```

- Step 1(Plan 13 完了後): グリフ統一確認、hide_idle_badge、category {badge}/{count}、dead config 配線、summary 置換 → 案A完成
- Step 2(Plan 15 完了後推奨): `vt statusline attention`(triage 集合に相乗り)
- Step 3: heartbeat / stale、status-interval 文書化、クリック
- Step 4(opt-in / 縮退): 案B state_colors、案D current-only、option bus 移行

実装計画は sidebar ロードマップ(Plan 13〜17)完了後に Plan 18 として起こす想定。dotfiles(M7 の `vtm`→`vt` 切替)とはプレースホルダ互換を保つ。

## 7. 評価反映版の実行計画(Codex 評価反映後・確定版)

Codex による評価(2026-07-05)を反映した確定計画。§6 の Step は本節で置き換える。

> 実装計画書: 統括は `docs/plans/2026-07-05-statusline-redesign-roadmap.md`。個別計画は Plan 19(Step 1)/ 20(Step 2)/ 21(Step 3)/ 22(Step 4)。Step 5 はロードマップ内の再評価ゲートとして扱い、実装計画は作らない。Plan 18 は sidebar レビュー指摘の修正用に予約。

### 7.1 評価の反映内容

**受け入れた指摘**

- 事実訂正: `outer_bg` の「削除は破壊的」は誤り。`SegmentColors` に `deny_unknown_fields` は無く(config/mod.rs:87-93)、削除しても既存 config は黙殺されるだけ。判断は**削除側に反転**(§5 更新済み)
- 「Plan 13 で自動単幅化」は session badge に限定(§2 更新済み)。agent-badge の `running:N` は summary 置換、suffix 既定は明示変更が必要
- 案A の規模は 小 → **中**。category `{badge}` はグリフ文字列→BadgeState の逆引きか構造化経路が必要で **中〜大** のため Step 5 へ後送り
- heartbeat 閾値は固定5秒でなく **`max(5s, poll_ms × N)` または設定値**(`daemon.poll_ms` が可変のため)
- Step 1 の分割(レビュー可能な単位へ)

**反論のうえ補強・維持した点**

- **案C fallback の再現度は Codex の評価より高い**: `wait_reason` と `started_at`(経過)は PaneSnapshot のフィールドとして tmux pane option から読めるため、daemon 停止時の fallback でも表示できる。daemon 限定なのは **unread のみ**(`pane_was_idle` 履歴依存)
- **category `{badge}` は Step 5 で「再評価」ではなく「不要判定が濃厚」**: Step 4 の attention チャネル(session 名指し)が入った時点でカテゴリ粒度の不在検知は下位互換になる。実装前提で持ち越さず、「まだ欲しいか」を問うゲートとする

### 7.2 確定 Step

1. [x] **Step 1(Plan 13 完了後)**: session badge 単幅化の確認、`session_badge.suffix` 既定を `""` へ、`hide_idle_badge` 追加(既定 false = ○ を表示。無印 = agent なしに純化)、current session の既定スタイル(bold + 色)
2. [x] **Step 2**: `vt statusline summary` 新設(`▲2 ●1` 形式・0件省略・tmux 色マークアップ付き)で `statusline-agent-badge` を置換。`agent_badge.enabled` をこの gate として配線
3. [x] **Step 3(案A-lite)**: `badge_style: inline | plain`(既定 inline。compartment は後続 opt-in)、category `{count}` プレースホルダ
4. [x] **Step 4(Plan 15 完了後)**: `vt statusline attention`(daemon Query + list-panes fallback の2段構え。fallback でも wait_reason / 経過は表示可、unread のみ daemon 限定)。同時に heartbeat(`@vde_heartbeat`、閾値 `max(5s, poll_ms × 3)`)+ stale 時の dim `?` 表示、`status-interval 1` 推奨の README 文書化
5. **Step 5(再評価ゲート)**: category `{badge}`(不要判定が濃厚)、案B `state_colors`、案D current-only モード、option bus 移行、`outer_bg` 削除、クリック対応(`--click`)

再評価メモ: attention の「最古」と elapsed は `started_at`(最終プロンプト送信時刻)基準で運用し、blocked 遷移時刻基準への変更は実運用を見て Step 5 で判断する。

### 7.3 DoD(Plan 18 起草時に詳細化する受け入れ条件)

**機能完了条件**

- [ ] Step 1〜4 の項目が実運用の tmux.conf(pill 装飾付き)で表示崩れなく動作する
- [x] 既定 config で current session が視覚的に区別でき、idle session に ○ が付き、agent なし session が無印になる
- [x] daemon 停止時: sessions/category は動作継続、summary/attention は stale 表示(dim `?`)に落ちる
- [x] 既存 dotfiles のプレースホルダ・prefix/suffix 設定が無変更で動く

**テスト完了条件**

- [x] `rtk cargo test` 全通過(summary / attention / badge_style / hide_idle_badge / heartbeat のユニットテスト含む)
- [x] `rtk cargo clippy --all-targets` 警告ゼロ、`rtk cargo fmt --check` 通過

**運用反映条件**

- [ ] `docs/e2e-smoke.md` に statusline の確認手順(summary・attention・stale)を追記し、smoke 実施を記録
- [x] README に `status-interval 1` 推奨・live/notify ならぬ summary/attention の設定例・suffix 既定変更の注意を記載
- [x] dotfiles 切替(M7)の手順に summary コマンド名の変更を反映
