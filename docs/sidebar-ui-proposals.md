# vde-tmux sidebar 表示形式の再設計案(評価用ドキュメント)

- 基準コミット: f7587a3
- 作成: Claude Code(現行実装調査 + herdr.dev / hiroppy/tmux-agent-sidebar の分析に基づく)
- モックはすべて幅40列(現行デフォルト `SidebarWidth::Columns(40)`)前提
- 本ドキュメントは Codex によるセカンドオピニオン評価のための資料。HTML 版(モック色付き)の内容を Markdown に落としたもの

## 0. 現状の表示と課題

ByRepo モード・filter all・幅40列での現行レンダリング(render.rs 準拠)。Chat 行選択 + Detail 展開状態:

```
repo     all
  v vde-tmux main +2 [running:3]
>   v 🟡 claude: fix sidebar flicker 2/5
      fix the flicker when an agent f
      status: running
      elapsed: 13m36s
      session: dev / pane: %12
      ├ Explore #a1b2
      └ general-purpose #c3d4
      -> jump
    🔴 codex: review PR #42
    🟢 claude (%15)
  v obsidian-sync main [waiting:2]
    🔴 codex: sync vault
    🔵 claude: update readme
```

rail モード(幅2以下)はバッジ絵文字のみを縦に並べる。

### 課題(主張)

構造(daemon push、rollup min() 集約、attention フィルタ、rail)は良い。課題は文字列の並べ方に集中:

1. **余白ゼロ** — 全行が pane 境界に密着
2. **絵文字バッジの幅問題** — 🔴🟡 は端末により 1〜2 セルで揺れる。彩度も高い
3. **右端が揃わない** — `[running:2]` や `2/5` がラベル直後に付き、状態・数値の縦スキャン軸がない
4. **truncate がぶつ切り** — `truncate_width` は省略記号なし・`chars().count()` 計測(表示幅でない)
5. **Detail 展開が重い** — 1 agent の詳細に 9 行
6. **ヘッダーが状態表示のみ** — `repo     all` は固定幅パディングが露出、クリック可能に見えない
7. **フッターなし** — キーが発見できない

## 1. 先行事例

### herdr(herdr.dev)

- 2段サイドバー: 上段 spaces(repo+branch)、下段 agents(名前 + 状態 · agent種別 + 状態グリフ)
- 状態は4値(blocked / working / done / idle)。人間の次の行動に1:1対応する粒度
- 1 agent = 1行に徹し、prompt すら出さない

### hiroppy/tmux-agent-sidebar(以下 tas)

- フィルタバー: 最上部に `≡6 ●1 ◎1 ◐2 ○2 ✕0`(アイコン+ライブ件数)。クリック / h/l / Tab でフィルタ切替。その下に repo セレクタ `▾`(r キー)
- 単幅グリフ5種: `●` running / `◎` background / `◐` waiting / `○` idle / `✕` error。競合時は running > permission > background > waiting > idle で解決
- 可変高カード: 状態行 + `❯` prompt + `▷` 最新レスポンス1行 + task グリフ `✔✔◼◻◻ 2/5` + subagent tree
- worktree 統合: sidebar から worktree+agent を spawn、1キーで window/worktree/branch を teardown
- デスクトップ通知: 完了・permission・エラー時

## 2. 前提: 状態グリフの共通刷新(全案共通)

絵文字バッジ → 単幅グリフ + ANSI 色。BadgeState 4値との対応:

| グリフ | 状態 | 現行 | 由来 |
|---|---|---|---|
| `▲` 赤 | blocked | 🔴 | permission / error / waiting |
| `●` 緑 | working | 🟡 | running / background |
| `✓` シアン | done | 🔵 | idle + unread |
| `○` 無彩 | idle | 🟢 | idle(既読) |

意味論変更: working=緑(健全)、blocked=赤(要対応)、done=シアン(未読)、idle=無彩。緊急度と彩度を一致させる。
代替: tas 型5値直接表示(`✕`/`●`/`◐`/`◎`/`○`)。attention 駆動なら4値、内訳常時表示なら5値。

## 3. 提案(A〜E)

### 案A: Refined Tree — 現行構造の視覚刷新

```
 repo · all                ▲2 ●1 ○2
 ────────────────────────────────────
 ▾ vde-tmux · main ↑2            ▲1
   ● claude  fix sidebar flicker…     ← 選択行(bg ハイライト)
     13m · task 2/5 · sub 2   ⏎jump   ← 選択行の直下のみ inline meta
   ▲ codex   review PR #42      perm
   ○ claude  —                   idle
 ▸ obsidian-sync · main           ▲1
 ────────────────────────────────────
 j/k移動 ⏎jump ␣開閉 ⇥attention
```

- 全行に左右1列 padding。`"> "` 選択マーカー廃止(bg + bold に一本化)
- 右端整列カラム(repo 行=attention 件数、chat 行=状態略語/経過)。`[running:2]` 廃止
- Detail 9行 → 選択行 inline meta 1行(Space フル詳細は残す)
- ヘッダー2ゾーン(左=モード・フィルタ、右=状態サマリ)+ フッター1行
- truncate は `…` + unicode-width
- Pros: render.rs 中心で完結、操作モデル不変 / Cons: attention が repo 内に散る
- 規模感: 小

### 案B: Two-Deck — herdr 型2段

```
 SPACES                             3
 vde-tmux       main ↑2          ▲   ← 選択
 obsidian-sync  main             ▲
 dotfiles       main             ·
 ────────────────────────────────────
 AGENTS · vde-tmux              all
 ● claude   working · 13m
   └ fix sidebar flicker      2/5
 ▲ codex    permission
   └ review PR #42
 ○ claude   idle · %15
 ────────────────────────────────────
 ⇥デッキ切替 ⏎jump a:全repo表示
```

- 上段 SPACES=repo 一覧(名前/branch/最悪状態1グリフ)、下段 AGENTS=選択 repo の agent(2行/agent)
- Tab でデッキ間フォーカス移動(現行 Tab=フィルタは f へ移設)。`a` で全 repo 横断表示(右端に repo 名)
- Pros: 折りたたみ操作が消える、上段が常時ダッシュボード、ネスト最大1 / Cons: 2フォーカスモデルで改修最大、repo 1〜2個だと上段が無駄
- 規模感: 大

### 案C: Attention Inbox — 状態バケツ再グルーピング

```
 inbox                     6 agents
 ▍ATTENTION 2
 ▲ codex  review PR #42        perm   ← 選択
   vde-tmux · 2m
 ▲ codex  sync vault           wait
   obsidian-sync · 8m
 ▍RUNNING 1
 ● claude fix sidebar flicker…  13m
   vde-tmux · task 2/5 · sub 2
 ▍QUIET 3
 ✓ claude update readme    obsidian
 ○ claude —                    vde
 ────────────────────────────────────
 ⏎jump n:次のattention ␣QUIET開閉
```

- 第一軸を repo → 状態バケツに: ATTENTION(blocked)→ RUNNING(working)→ QUIET(done+idle)。BadgeState の Ord がセクション順
- agent は2行(1行目=グリフ+agent+prompt+右端状態、2行目=repo・経過・進捗 dim)
- QUIET はデフォルト折りたたみ可。**ViewMode の4つ目(ByStatus)として追加**
- Pros: 要対応が構造的に最上部、フィルタ切替不要に / Cons: repo 文脈が弱い、状態変化で行が動く
- 規模感: 中

### 案D: Dense Monitor — 1行1エージェント

```
 agents 6  ▲2 ●1 ✓1 ○2     [all]
 ▲ codex   vde review PR #42  perm
 ▲ codex   obs sync vault     wait
 ● claude  vde fix sidebar f… 13m    ← 選択
 ✓ claude  obs update readme    ✓
 ○ claude  vde —                ·
 ○ claude  dot —                ·
 ────────────────────────────────────
 ● claude · vde-tmux · %12 · 13m36s   ← 下部ステータスペイン(2行固定)
 task 2/5 · sub: Explore, general…
```

- 緊急度ソートの1行リスト(Flat 強化)。固定カラム: グリフ / agent名7字 / repo略号3字 / prompt可変… / 右端状態
- 詳細は下部ステータスペイン常時表示(Detail 行・Space 開閉が消える)
- 幅24列まで自然縮退
- Pros: 密度最大(24行で約20体) / Cons: repo 文脈が3字略号のみ、prompt 幅最小
- 規模感: 中

### 案E: Rich Card — tas 型情報最大化

```
 ≡6 ▲2 ●1 ✓1 ○2       repo ▾ —
 ────────────────────────────────────
 ● claude  main +             13m    ← 選択
   ❯ fix sidebar flicker
   ▷ Found it. The rollup recompu…
   ✔✔◼◻◻ 2/5
   ├ Explore #a1b2
   └ general-purpose #c3d4
 ▲ codex   main       permission
   ❯ review PR #42
 ▲ codex   main +         waiting
   ❯ sync vault
 ○ claude  main
 ────────────────────────────────────
 ⏎jump w:worktree生成 x:破棄
```

- フィルタバー(icon+count、クリック/h/l で多値フィルタ)+ repo セレクタ
- 可変高カード: 状態行(グリフ・agent・branch・worktree `+`・右端経過/状態)+ `❯` prompt + `▷` 最新レスポンス + task グリフ + subagent tree。idle は1行縮退
- worktree 統合(`w` spawn / `x` teardown、vde-worktree 連携)
- Pros: jump せず進行が分かる唯一の案、フィルタバーの情報効率最高 / Cons: 密度最低(24行で約5体)、40列では ▷ が切れる、▷ の取得機構が daemon にない(hook 拡張 or capture-pane 要)
- 規模感: 大(段階導入可)

## 4. 比較表

| 観点 | 現状 | A | B | C | D | E |
|---|---|---|---|---|---|---|
| 1画面のagent数(高さ24行) | 〜10 | 〜12 | 〜7(下段) | 〜8 | 〜20 | 〜5 |
| attention の発見速度 | ソート依存 | バッジ+ソート | 上段ロールアップ | 構造そのもの | 最上部固定 | フィルタバー件数 |
| repo の文脈把握 | ツリー | ツリー | 上段専任 | 行内ラベルのみ | 3字略号のみ | branch+セレクタ |
| 詳細情報への到達 | 9行展開 | 選択行inline 1行 | 2行目+preview | 2行目固定 | preview頼み | 常時行内(▷含む) |
| 狭幅(30列)耐性 | 崩れる | meta省略で可 | 下段のみ可 | 2行目省略で可 | 最適 | 低い |
| 実装コスト | — | 小(render中心) | 大(state/tree/入力) | 中(tree差替) | 中(Flat拡張) | 大(daemon拡張要) |
| 現行stateとの互換 | — | 全互換 | 選択モデル変更 | ViewMode追加 | Flat拡張 | データ源の追加 |

## 5. 横断デザイン指針

- **余白**: 全行に左右1列 padding(rail 時のみなし)。選択は bg フル幅塗り + bold に一本化(`"> "` マーカーとの二重表現廃止)。セクション境界は罫線より空行優先(罫線はデッキ境界のみ)。インデント2スペース・ネスト最大2段
- **右端カラム**: 左端グリフ列と右端状態・数値列を固定カラム化し、縦スキャンの第2軸を作る。数値は右揃え
- **truncate**: `…` 必須、unicode-width 計測(CJK/絵文字で右端が崩れる現行の潜在バグも解消)。切るのは中央可変テキストのみ
- **rail 強化**: 幅2→4列にし「状態別カウント + 個別(グリフ+repo頭文字)」の2部構成。attention 発生時に点滅(reverse video)

## 6. 機能案(表示形式と独立、優先度順)

| 優先 | 機能 | 内容 | 足場 |
|---|---|---|---|
| 高 | フィルタバー icon+count 化(tas由来) | `≡6 ▲2 ●1 ✓1 ○2`。サマリ表示とフィルタ操作の統合、多値化 | StatusFilter 拡張 + 既存ヘッダー hit test |
| 高 | n/N 次の attention へ | フィルタ切替なしで blocked 間巡回 | SidebarAction 追加のみ |
| 高 | unread 既読管理 | done は jump 時点で idle 表示へ | unread フラグは daemon に既存 |
| 高 | 状態変化フラッシュ | 変化行を1〜2秒 reverse video | frame に changed_at |
| 中 | task 進捗グリフ化(tas由来) | `✔✔◼◻◻ 2/5` | parse_tasks 済みだが per-task 状態は hook 拡張要 |
| 中 | / インクリメンタル絞り込み | agent名・prompt・repo名の部分一致 | query: Option<String> |
| 中 | フッターのコンテキストヒント | 選択行種別でヒント変更 | SidebarRowKind 分岐 |
| 中 | 経過時間 live 更新 | push なしでも毎分更新 | event loop に tick |
| 中 | worktree 連携(tas由来) | w spawn / x teardown / `+` マーカー | vde-worktree に接続のみ |
| 低 | ▷ レスポンスプレビュー(tas由来) | 最後の発言1行 | hook で @vde_last_response、capture-pane でも代替可 |
| 低 | デスクトップ通知(tas/herdr由来) | blocked/error/done 遷移時 | daemon 遷移検知に osascript 等 |
| 低 | pin(agent 先頭固定) | manual_order の agent 版 | — |

## 7. 推奨(コスト考慮時)

**案A を基盤 + tas 由来フィルタバー + 案C を ViewMode::ByStatus として追加。**

- Phase 1: 案A 土台(グリフ・padding・右端カラム・truncate・フッター・フィルタバー)
- Phase 2: inline meta、n ジャンプ、既読管理、状態変化フラッシュ、task グリフ
- Phase 3: ViewMode::ByStatus(4キー)。attention 運用主体なら既定に
- Phase 4: worktree 連携
- Phase 5: 必要に応じ案B/D/E 再評価

## 8. 案F: 理想形 Command Center(実装コスト度外視)

設計原理3つ: **①モード廃止(状況が表示を決める) ②fisheye(選択だけリッチ) ③サイドバーから出ない**

```
 ≡6 ▲2 ●1 ✓1 ○2       repo ▾ —        ← ①フィルタバー
 ────────────────────────────────────
 ▍TRIAGE 2                             ← ②要対応ゾーン(attention 0 なら消滅)
 ▲ codex · vde-tmux       perm 2m      ← 選択
   $ cargo sqlx migrate run
   [y]許可 [n]拒否 [d]diff [⏎]jump     ← 行内アクション
 ▲ codex · obsidian-sync   wait 8m
 ────────────────────────────────────
 ▾ vde-tmux · main ↑2                  ← ③FLEET(repo別)
 ● claude  fix sidebar flicker…  13m
 │ ▷ Found it. The rollup recompu…     ← 選択/pin 行のみ fisheye 展開
 │ ✔✔◼◻◻ 2/5 · ├Explore └general…
 ○ claude  —                       ·
 ▾ obsidian-sync · main
 ✓ claude  update readme            ✓
 ────────────────────────────────────
 LIVE · codex %14                      ← ④選択 agent の画面末尾を実況
 │ Apply pending database migratio
 │ ❯ 1. Yes  2. No, and tell Claud
 ────────────────────────────────────
 y:許可 n:次▲ /検索 w:worktree ?:keys
```

### 原理① モード廃止

幅・高さ・agent 数・attention 有無から表示密度を自動導出。切替キーなし。

| 幅 | 形態 |
|---|---|
| ≥56 | rich(全行カード、案E相当) |
| 36–55 | standard(上記モック) |
| 24–35 | dense(1行/agent、案D相当) |
| 7–23 | micro(グリフ+repo頭字+分数) |
| ≤6 | rail(集計+個別グリフ) |

attention 0 なら TRIAGE 消滅、repo 1つなら repo 見出しも消える。A〜E は同一 UI の断面になる。

**スコープ(category / repo)の扱い**: 素の状態は全 category・全 session 横断(現行 daemon の収集範囲と同じ)。

- TRIAGE は常にグローバル。スコープ絞り込み中でも attention はスコープ外から隠れない(行内に `codex · work/vde-tmux` とスコープを表示)
- FLEET はフィルタバー右端の `scope ▾`(category → repo の2段選択)で絞れる。`—` なら全横断
- category 見出しは自動適応: category が1つなら非表示、複数あるときだけ repo 見出しに dim で前置(`▾ work/vde-tmux · main`)。ネストは深くしない(インデント最大2段の指針を維持)

### 原理② Fisheye + pin(3段階の行高)

- **選択行**: フル展開(▷・task・subagent、2〜4行)。選択が離れたら自動で畳む
- **pin 行**(Space トグル、pin 印付き): 中展開(task グリフ+▷ の2行)を選択位置に関わらず維持。「見張りたい agent」用
- **その他**: 常に1行

pin をフル展開にしないのは密度防衛。高さ不足時は最後に触った pin から1行へ縮退(pin 状態は保持)。repo 見出しの開閉は現行どおり残す(関心の管理であり fisheye と競合しない)。

### 原理③ サイドバーから出ない

- **permission 行内応答**: TRIAGE で y/n → 対象 pane に send-keys。実行コマンドを行内常時表示、d で diff を floating preview。jump→応答→戻るの往復を1キー化
- **LIVE ペイン**: 選択 agent の pane 末尾2〜3行を capture-pane で常時表示。e でイベントログ(`2m前 codex → permission`)に切替
- worktree spawn/teardown、/ 検索、n/N 巡回でライフサイクルがサイドバー内で完結

### 代償と限界(自認)

- daemon 拡張要: last_response hook、capture-pane ストリーム、遷移イベントバッファ
- send-keys 応答は安全装置必須(送信前に対象 pane 状態を再確認、レース対策)
- 自動変形の境界値はユーザー調整可能に(勝手に変わる不快感)
- fisheye は行高が動く → 展開が viewport 内に収まるスクロール制御必須
- 行内応答は「読まずに y を叩く」誘惑 → コマンド行の常時表示は外せない
- 段階導入と両立: A → フィルタバー → TRIAGE 分離(C)→ LIVE ペイン → fisheye → 行内応答 の順で全段が中間状態として運用可能

## 9. 確定実行計画 — F案(Command Center)をゴールとする段階導入

改訂履歴:

- v1(2026-07-05): Codex 評価反映版。A+C+軽量 live を着地点とする保守案
- v2(2026-07-05): ユーザー判断により **F案を明示的なゴールに再設定**。send-keys 不採用と表示基盤に関する Codex の指摘は維持しつつ、Phase 構成を「後から F案へ寄せる際に手戻りが出ない順序」へ組み替え

§7 の Phase 案は本節で置き換える。ゴールは §8 の F案から次の2点を除いたもの:

- **permission send-keys 行内応答は不採用確定**(jump & return で代替。再評価対象外)
- **モードの完全廃止は保留**。当面は明示 ViewMode を併存させ、F レイアウト定着後に使用実測を見て退役を判断する

### 9.1 Codex 評価の反映内容(v1 時点の記録)

**受け入れた指摘**

- 事実修正: Detail 展開は「9行固定」ではなく可変(3行〜)。「余白ゼロ」は右側と視覚 padding の問題(左は `"> "` 2文字が既存)
- 案A のコストは 小 → **中**。inline meta には `SidebarRow` への構造化データ追加(elapsed / tasks / subagent 数を label と分離)が必要
- 案E の daemon 影響(protocol / workers / snapshot)は原案で過小評価だった
- Phase 分割はレビュー可能な単位に細分化(原案 Phase 1 は過大)
- task 進捗グリフは per-task 状態の hook 拡張に依存するため後送り
- 状態グリフは 4値(BadgeState)を主とし、RollupLevel の内訳は右端カラム・詳細テキストで出す
- モード廃止(F 原理①)は撤回し、**明示 ViewMode 維持 + モード内の幅適応密度**へ変更
- permission への send-keys 行内応答は**採用しない**

**反論のうえ維持した点**

- **working=緑 を維持**(Codex は黄 or 青を提案)。根拠: 現行 `RollupLevel::Running` の行テキスト色が緑であり、バッジのみ黄にするとグリフと行テキストで色が不一致になる。また黄は `RollupLevel::Waiting` のテキスト色と衝突する。「完了」との誤読は done=シアン + ✓ の形状差で防ぐ。黄へ変える場合は rollup 色の再割り当てとセットが条件
- send-keys の代替として **jump & return** を採用(下記 Phase 2)。permission 往復の短縮という F案の価値を、入力送信リスクなしで回収する

### 9.2 確定 Phase(v2)

> 実装計画書: 全 Phase の統括は `docs/plans/2026-07-05-sidebar-redesign-roadmap.md`。個別計画は Plan 13(Phase 1)/ 14(Phase 2)/ 15(Phase 3)/ 16(Phase 4)/ 17(Phase 5)。Phase 6 はロードマップ内の再評価ゲートとして扱い、実装計画は作らない。

設計上の要諦は2つ。**(1) Phase 1 の時点で行レンダリングを「幅ティア × 行高ティア」でパラメタ化した構造にする**(後段の fisheye・幅適応で描画パスを作り直さないため)。**(2) Phase 3 は切替式 ByStatus ビューではなく TRIAGE 常設ゾーンとして作る**(F案との最大の分岐点。切替式で作ると F案へ寄せる際に作り直しになる)。

1. [x] **Phase 1: 表示基盤(F対応設計)**
   - 単幅グリフ(▲●✓○、BadgeGlyphs 既定値変更。設定で絵文字に戻せる)
   - unicode-width ベースの truncate + `…`
   - 全行の左右 padding、選択表現の一本化(bg + bold、`"> "` 廃止)
   - フッター1行(キーヒント)、ヘッダーの固定幅パディング露出解消
   - **設計制約**: 行フォーマット関数は幅ティア(rich / standard / dense / micro / rail)を引数に取る構造にする(初期実装は standard + rail のみ)。`SidebarRow` に構造化フィールド(elapsed / tasks / subagents / wait_reason)を追加し、表示 label への埋め込み依存を解消する
   - フィルタは既存 All/Attention の2値のまま
2. [x] **Phase 2: fisheye 第一段 + 往復支援**
   - Detail の既定展開をやめ、選択行直下に inline meta 1行(= fisheye「選択=展開」の最小形)。meta 行は daemon 側で通常の SidebarRow として挿入し、クリックの行対応(1:1)を維持する
   - `n` / `N` attention 巡回
   - jump 時の unread 即時既読化(現行は次ポーリングで window_active を見て解除する遅延型。runtime.rs の JumpPane 分岐に即時 clear を追加する差分のみ)
   - **jump & return**: jump 後に1キーでサイドバーへ帰還。sidebar TUI はフォーカス外ではキーを受けられないため、`vt sidebar focus` サブコマンドを新設し、ユーザーの tmux.conf で bind する方式(ドキュメントに設定例を記載)
3. **Phase 3: TRIAGE 常設ゾーン + FLEET(F案の骨格)**
   - 切替式 ByStatus ビューは作らない。blocked を常設 TRIAGE ゾーンとして最上部に分離し、残りを FLEET とする
   - TRIAGE はスコープ・フィルタを貫通して常にグローバル(0件で自動消滅)。行内に出所(category/repo)を表示
   - 既存 ViewMode(flat/repo/category)は FLEET 部分のグルーピングとして存続
   - ゾーンを跨ぐ選択移動・クリック判定
4. **Phase 4: fisheye 完成(pin + 3段階行高)+ 幅適応**
   - 選択行=フル展開(task・subagent・meta。▷ はまだ含めない)、pin(Space)=中展開、その他=1行
   - 可変行高に対応する scroll 制御(展開行を viewport 内に保持)とクリック hit-test
   - 幅ティア dense / micro の実装(狭幅での自動縮退)、rail の「集計 + 個別」2部構成化
5. **Phase 5: LIVE ペイン + フィルタバー多値化**
   - 選択 pane の tail のみ・低頻度・TUI 側 transient 表示(daemon snapshot に混ぜない)。`e` でイベントログ(状態遷移フィード)に切替
   - フィルタバー icon+count 化(`≡ ▲ ● ✓ ○` + 件数、多値フィルタ)
   - 状態変化フラッシュ、経過時間の live tick、デスクトップ通知(opt-in)
6. **Phase 6: 拡張・再評価ゲート**
   - `▷` 最新レスポンス表示(hook / daemon 拡張が必要なため最後)
   - worktree 連携(`w` spawn / `x` teardown / `+` マーカー)
   - rich ティア(幅 ≥56 での全行カード)
   - ViewMode 退役の判断(使用実測に基づく)
   - permission send-keys は再評価対象外(不採用のまま)

### 9.3 DoD(Definition of Done)

各 Phase 共通の完了条件。Phase 単位でこのチェックリストを満たしたらマージ可能とする。

**機能完了条件**

- [ ] 当該 Phase の項目がすべて幅40列・高さ24行で表示崩れなく動作する
- [ ] 幅30列・rail(≤2列)でも既存機能が退行しない(Phase 1 以降は `…` truncate が全幅で機能する)
- [ ] CJK を含む prompt / repo 名で右端カラムが崩れない(Phase 1 以降)
- [ ] 既存の設定カスタマイズ(BadgeGlyphs / SidebarColorsConfig / SidebarHeaderConfig)で旧表示に戻せる
- [ ] (Phase 3 以降)TRIAGE ゾーンがスコープ・フィルタ設定に関わらず blocked を表示し、0件で自動消滅する
- [ ] (Phase 4 以降)可変行高の状態で、クリックの対象行判定と選択行の viewport 内保持が正しい

**テスト完了条件**

- [ ] `cargo test` 全通過(既存テストの期待文字列更新を含む)
- [ ] 追加・変更した描画パスにユニットテストがある(render の行フォーマット、truncate 境界、ゾーン行構築)
- [ ] (Phase 3 以降)ゾーン跨ぎの選択移動・TRIAGE 貫通のテストがある
- [ ] (Phase 4 以降)可変行高の hit-test / scroll 制御に回帰テストがある
- [ ] `cargo clippy` 警告ゼロ

**運用反映条件**

- [ ] `docs/e2e-smoke.md` の該当手順を更新し、smoke を実施して結果を記録
- [ ] キーバインド変更がある場合はフッターヒントと README/docs の記載が一致
- [ ] 本ドキュメント §9.2 の当該 Phase にチェックを付け、差分・判断変更があれば追記
