# vde-tmux

[English](./README.md) | **日本語**

vde-tmux は、tmux で動かしている AI コーディングエージェントの状態を一覧できるツールです。
Claude Code、Codex、opencode の pane を追跡し、tmux の status line とサイドバーへ状態を表示します。

![vde-tmux sidebar](https://github.com/user-attachments/assets/e912448f-b657-49d9-b175-39a0cbad04f2)

## できること

- すべての tmux session にいるエージェントを `Blocked`、`Limited`、`Working`、`Done`、`Idle` に分類する
- 対応が必要なエージェントを status line に表示する
- task 要約、経過時間、task、subagent、worktree activity をサイドバーに表示する
- サイドバーからエージェントの pane へ直接移動する
- session をカテゴリで整理し、キーボードや status line のクリックで切り替える
- エージェントが入力待ちになったとき、任意の通知コマンドを実行する

## 必要なもの

- tmux 3.2 以降
- 最新の stable Rust と Cargo（インストールに使用）
- `PATH` にある git、lsof
- 任意：session manager を使う場合は fzf、project selector を使う場合は ghq

## インストール

```bash
cargo install vde-tmux --locked
```

`vt` と `vde-tmux` の二つの同等なコマンドがインストールされます。
以降は短い名前の `vt` を使います。

```bash
vt --version
```

## セットアップ

### 1. tmux の設定

`~/.tmux.conf` に次の設定を追加します。

```tmux
run-shell -b 'vt daemon ensure'

set -g status-left-length 10000
set -g status-right-length 80
set -g status-left '#{@vde_status_summary}#[fg=#8f8ba8] │ #[default]#{@vde_status_category}#[fg=#8f8ba8] │ #[default]#{@vde_status_sessions} #{@vde_status_attention}'
set -g status-right '#{@vde_status_windows}'

setw -g window-status-format ''
setw -g window-status-current-format ''
set -g window-status-separator ''

set -g pane-border-status bottom
set -g @vde_status_now_format '%s'
set -g pane-border-format '#{?#{@vde_status_pane},#{E:@vde_status_pane},#{pane_index} #{pane_current_command}}'

bind-key -n MouseDown1Status run-shell "vt statusline-click --client-name #{q:client_name} --session-id #{q:session_id} #{q:mouse_status_range}"
bind-key -n M-h run-shell "vt session-cycle prev --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-l run-shell "vt session-cycle next --client-name #{q:client_name} --session-id #{q:session_id}"
bind-key -n M-e run-shell "vt sidebar focus-toggle --window #{q:window_id}"
```

設定の要点は次のとおりです。

- `vt daemon ensure` が daemon を必要に応じて起動します。
- vde-tmux は描画済みのテキストを `@vde_status_*` option へ書き込むため、status line の再描画ごとに外部プロセスは起動しません。
- `@vde_status_now_format` は pane border の経過時間表示に必要です。
- `Blocked`、`Limited`、`Working`、`Done` の agent pane は、下辺の pane border の残り幅をバッジと同じ色の線で埋めます。
- `window-status-*` の設定は、tmux 標準の window list を vde-tmux の session と window の表示へ置き換えます。
- Window マップは `status-right` に置き、Session 一覧が長い場合も現在の Window を表示します。`status-format` を上書きしている場合は、右寄せの `#{T:status-right}` 領域も含めてください。
- `--client-name` と `--session-id` により、複数の tmux client を使っていても操作対象が別の client へずれません。

設定を読み込み直します。

```bash
tmux source-file ~/.tmux.conf
```

### 2. Neovim の pane navigation（任意）

この repository は Neovim plugin も提供します。lazy.nvim では次のように読み込みます。

```lua
{
  'yuki-yano/vde-tmux',
  lazy = false,
  config = function()
    require('vde-tmux').setup()
  end,
}
```

デフォルトの `<C-h/j/k/l>` は Neovim 内では window 間を移動し、端では daemon に隣の tmux pane への移動を依頼します。
移動先の pane が Neovim の場合は、移動元のカーソル位置に合う window を選択します。

`require('vde-tmux').navigate('h')` のように API だけを既存の mapping から呼ぶこともできます。`setup()` には `keybindings = false`、`modes`、`debug`、`disable_when_floating`、`navigate_from_floating` を指定できます。

daemon は常駐する一つの tmux control-mode client を通して pane を切り替えます。
vde-tmux は session に client が attach しているかを判定するときにこの client を除外しますが、`tmux list-clients` には表示されます。
通常の client が一つも attach していない間は、この client の attach 先 session について、tmux 自身の alert と `destroy-unattached` は attach 中として扱います。

### 3. Claude Code の hook

`~/.claude/settings.json` に次の hook を追加します。

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "vt hook claude SessionStart" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "vt hook claude UserPromptSubmit" }] }],
    "PreToolUse": [{ "hooks": [{ "type": "command", "command": "vt hook claude PreToolUse" }] }],
    "PostToolUse": [{ "hooks": [{ "type": "command", "command": "vt hook claude PostToolUse" }] }],
    "Notification": [{ "hooks": [{ "type": "command", "command": "vt hook claude Notification" }] }],
    "Stop": [{ "hooks": [{ "type": "command", "command": "vt hook claude Stop" }] }],
    "StopFailure": [{ "hooks": [{ "type": "command", "command": "vt hook claude StopFailure" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "vt hook claude SessionEnd" }] }]
  }
}
```

保存後に Claude Code を再起動すると、状態遷移と task の進捗が表示されます。

Claude の親 Bash tool が背景実行した `vt agent wait`、`vt agent run wait`、`vt agent operation wait` は、通知の配達または成功した `TaskStop` と、その後の親応答の `Stop` まで Working を維持します。個別 pane には Working の色で `◌` と「結果待ち」を表示します。記号は `badge.glyphs.awaiting_result` で変更でき、Working の件数とフィルタにも含まれます。

対象は単独のリテラル command です。実行ファイルは `vt` または実行中の vt と同じ絶対パスに限ります。変数、shell 演算子、redirect、shell/Python 経由の間接呼び出しは対象外です。自動の背景移行も追跡しますが、常駐 server など他の背景 command は run を保持しません。hook は同期で実行し、`async: true` を設定しないでください。観測した形式は Claude Code 2.1.287 で、利用する版で事前に確認します。出力本文から欠けた背景 task ID を推定しません。

API の `background_wait` と pane 詳細の一覧への有無・確認時刻は、最後の正常な `Stop` 時点の snapshot です。現在の実行状況や結果の配達を示すものではありません。通知が欠けた場合、時間だけでは完了させません。復旧できない場合は sidebar で対象 pane を選んで `d` を押し、手動完了にします。台帳を解除する操作で、command 自体は停止しません。Claude の UI から停止すると `TaskStop` と通知が出ないこともあり、その場合も手動完了が必要です。不正な receipt は `await_*` の理由付き Blocked とし、同じ原因で繰り返し通知しません。

展開した pane の `◷` は予約件数と最後の親 `Stop` の確認時刻を、task label の色で示します。API の `scheduled_crons` も同じ snapshot です。予約は主バッジや run の完了条件を変えず、計算中は表示しません。cron 発火時の prompt と Done 通知は通常の応答と同じ扱いです。手動・自動完了後に届いた task 通知も新しい応答として処理し、古い待機を復元しません。

### 4. Codex の hook

`~/.codex/hooks.json` または project の `.codex/hooks.json` に次の hook を追加します。
保存後、Codex の `/hooks` で内容を確認して承認します。

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume|clear",
        "hooks": [{ "type": "command", "command": "vt hook codex SessionStart" }]
      }
    ],
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "vt hook codex UserPromptSubmit" }] }
    ],
    "PermissionRequest": [
      { "hooks": [{ "type": "command", "command": "vt hook codex PermissionRequest" }] }
    ],
    "PostToolUse": [
      {
        "matcher": "^(update_plan|request_user_input_async)$",
        "hooks": [{ "type": "command", "command": "vt hook codex PostToolUse" }]
      },
      {
        "matcher": "^Bash$",
        "hooks": [{ "type": "command", "command": "vt hook codex PostToolUse" }]
      }
    ],
    "SubagentStart": [
      { "hooks": [{ "type": "command", "command": "vt hook codex SubagentStart" }] }
    ],
    "SubagentStop": [
      { "hooks": [{ "type": "command", "command": "vt hook codex SubagentStop" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "vt hook codex Stop" }] }
    ]
  }
}
```

tmux 内の Codex は `codex --no-daemon` で起動します。
vde-tmux はこの Embedded mode の hook だけを受け付けます。共有 app-server の hook は通知元の pane を特定できないため拒否します。
この指定はその起動だけに適用され、他のクライアントの共有 daemon 設定は変わりません。`features.daemon_auto_start=false` だけでは、起動済みの server への接続は防げません。
`codex queue` など共有 server を必要とするコマンドは `--no-daemon` では使えません。実行中の共有 thread を両方のモードで同時に開かないでください。

設定後は、permission request、plan、subagent、worktree activity、[Question 通知](#codex-の-question-通知)がサイドバーに表示されます。

hook が受理されていない Codex pane は、画面から読み取った状態を表示します。作業中なら Working、承認や同期的な質問なら Blocked です。
画面を判別できない場合は `?`（Unknown）を表示します。
画面からの判定で run を完了させたり、Question 通知を確認済みにしたりすることはありません。
バッジの判定理由は `vt agent get --json` の `summary.presentation` で確認できます。

### 5. 動作確認

tmux 内で次のコマンドを実行します。

```bash
vt daemon status
vt sidebar open
```

hook を設定していなくても、Claude Code、Codex、opencode は pane の実行コマンドから検出できます。
ただし、prompt、完了時刻、入力待ちを正確に表示するには hook が必要です。

## 状態の読み方

| 表示 | 状態 | 意味 |
| --- | --- | --- |
| `▲` | Blocked | 許可や回答など利用者の入力を待っている、またはエラーで止まっている |
| `⋄` | Limited | provider の利用量または session 上限に達している。要対応には含めない |
| `●` | Working | エージェントが作業している |
| `✓` | Done | 作業が完了し、まだ確認されていない |
| `○` | Idle | 作業がない、または完了を確認済み |
| `?` | Unknown | hook が受理されていない Codex pane の画面を判別できない |

`Done` は、対象の pane がいずれかの tmux client で active になると `Idle` になります。
同じ window の別 split を見ても既読にはなりません。
既読状態は daemon の再起動後も保持され、すべての tmux client とサイドバーで共有されます。

`unread-latest` は、全 pane のうち未読のイベント（入力待ち、エラー、完了）が最も新しい pane へ移動します。
移動操作そのものは既読化せず、移動先が active pane として観測された後に既読になります。

Limited は、Claude Code の `StopFailure` hook の `error=rate_limit`、または画面に表示された provider の利用上限メッセージから判定します。
Claude Code のそれ以外の API エラーは Blocked になります。
失敗した turn は途中で副作用が発生している可能性があるため、自動では再実行しません。
次の `SessionStart` または `UserPromptSubmit` を受け取るか、プロセスが終了すると Limited は解除されます。
provider のメッセージは `vt pane read` で確認できます。

## サイドバー

サイドバーは現在の tmux window に開きます。

```bash
vt sidebar open --width 40
vt sidebar open --width 20%
vt sidebar toggle
vt sidebar toggle --all
vt sidebar rail    # 細い rail 表示と通常幅を切り替える
vt sidebar close
```

`vt sidebar focus-toggle` は、サイドバーがなければ開き、表示中ならフォーカスし、フォーカス中なら閉じます。

### 表示

サイドバーには独立した二つの表示設定があります。

- 対象範囲：`Current` はサイドバーの session が属するカテゴリだけ、`All` はすべてのカテゴリを表示します。
- 表示方法：`Tree` は Current では Repository → Agent、All では Category → Repository → Agent の階層で表示します。`Priority` は Pinned、Needs Input、Questions、Limited、Unread Done、Running、Idle の順にまとめます。`Flat` はまとめずに並べます。

要対応（Needs action）フィルタには、Blocked のエージェントと、未確認の [Question 通知](#codex-の-question-通知)があるエージェントが含まれます。
未読の Done は Done フィルタに表示されます。

`p` で pin したエージェントは、Priority と Flat では先頭に、Tree では所属するカテゴリと Repository ごと上位に表示されます。
pin は未読、バッジ、通知とは独立しており、pane が消えると解除されます。

アクティブな session に属するエージェントには、左端に水色の `▎` を表示します。
tmux client がフォーカスしているエージェントの pane には、代わりに黄色の `selection_bar` の色を使います。
キーボードでの選択は行の背景色で示します。
editprompt の editor pane は、`@editprompt_is_editor`、`@editprompt_target_panes`、`@editprompt_editor_pane` の option で双方向に結び付いている場合、対象エージェントをフォーカスしているものとして扱います。

Repository と linked worktree の branch label には、upstream との差を `↑N` / `↓N`、`HEAD` からの staged と unstaged の差分行数を `+N` / `-N` で表示します。
Git の ignore 対象ではない untracked なテキストファイルも `+N` に含めます。
0 件は省略し、binary file は数えません。

展開したエージェントには、branch または worktree、task の状態、ahead/behind、listen 中の TCP port、Claude Code が `run_in_background` として報告した background コマンド、最後の応答のプレビュー（`▷`）を表示します。

対象範囲、表示方法、フィルタ、手動の並び順、開閉状態、選択位置、スクロールは、同じ tmux server のすべてのサイドバーで共有します。
具体的な Current のカテゴリと戻り先は、サイドバーごとに起点 session に追従します。
対象範囲、表示方法、フィルタ、手動の並び順、開閉状態は、tmux socket ごとに `$XDG_STATE_HOME/vde/tmux/sidebar-state/` へ保存します。

### キー操作

| キー | 動作 |
| --- | --- |
| `j` / `k`、`↓` / `↑` | 行を移動する |
| `gg` / `G` | 先頭行または末尾行へ移動する |
| `Ctrl-D` / `Ctrl-U` | 半ページ下または上へ移動する |
| `Ctrl-F` / `Ctrl-B` | 1ページ下または上へ移動する |
| `Enter` | 選択したエージェントの pane へ移動する |
| `Space` | 選択行を開閉する |
| `c` | 対象範囲を Current / All で切り替える |
| `v` | 表示方法を Tree / Priority / Flat の順に切り替える |
| `1` / `2` / `3` | Tree / Priority / Flat へ直接切り替える |
| `Tab` / `Shift+Tab` | 状態フィルタを切り替える |
| `n` / `N` | 対応または Question 通知の確認が必要な、次または前の pane へ移動する |
| `p` | 選択中のエージェントを pin または unpin する |
| `d` | 選択中の run を完了としてマークする |
| `Q` | 表示中の Question 通知を確認済みにする。質問への回答やスキップは行わない |
| `a` | カテゴリ追加の dialog を開く |
| `m` | 選択中の Repository を移動する dialog を開く |
| `r` | 選択中の動的カテゴリの名前を変更する dialog を開く |
| `D` | 選択中の動的カテゴリを削除する dialog を開く |
| `J` / `K` | 手動の並び順を変更する |
| `q` / `Esc` | サイドバーを閉じる |

エージェントの1行目をクリックすると開閉し、2行目以降をクリックするとその pane へ移動します。
マウスホイールは選択位置を動かさずにスクロールします。

カテゴリの dialog では、`j`/`k`、矢印キー、`gg`/`G` で項目を選び、`Enter` で保存、`Esc` でキャンセルします。
保存に失敗した場合は dialog を開いたままエラーを表示します。

### tmux からサイドバーを操作する

開いているサイドバーは、フォーカスを移さずに操作できます。

```tmux
bind-key -n M-v run-shell "vt sidebar input v --window #{q:window_id}"
bind-key -n M-f run-shell "vt sidebar input tab --window #{q:window_id}"
bind-key -n C-M-j run-shell "vt sidebar input agent-next --window #{q:window_id} --client-pid #{client_pid}"
bind-key -n C-M-k run-shell "vt sidebar input agent-prev --window #{q:window_id} --client-pid #{client_pid}"
bind-key -n C-M-e run-shell "vt sidebar input read-current --window #{q:window_id} --client-pid #{client_pid}"
bind-key -n M-u run-shell "vt sidebar input unread-latest --window #{q:window_id}"
bind-key -n M-p run-shell "vt sidebar input pin-toggle --window #{q:window_id}"
```

`C-M-j` と `C-M-k` は Priority 表示でだけ動作します。
操作元の client を、表示中の次または前のエージェントへ端で折り返さずに移動し、未読のまま残します（peek）。
`C-M-e` は現在の peek 対象を既読にし、Priority 表示ではその下にある次の未読エージェントへ進みます。
peek の状態は操作元の client ごとに保持され、別の client が同じ pane を表示すると既読になります。
daemon を再起動すると peek は終了します。

### layout manager との連携

layout manager は、pane layout を適用する前にサイドバーの予約を同期的に確定できます。

```bash
vt sidebar prepare-layout --window @12 --json
```

このコマンドは
`{"window_id":"@12","status":"ready","reserved_panes":[{"pane_id":"%23","role":"sidebar"}],"content_anchor":"%22"}`
のような JSON object を一つ出力します。
`ready` は、返したサイドバー pane がすべて存在し、幅の調整が完了したことを表します。サイドバーの描画は待ちません。
auto-all hook が無効でサイドバーがない場合は、`absent`、空の `reserved_panes`、既存の content pane を `content_anchor` として返します。
auto-all の new-window hook と同時に実行しても結果は変わりません。

このコマンドは daemon を起動しません。
設定を反映した daemon が動いていない場合（先に `vt daemon reload` を実行してください）、window が不正または content pane がない場合、tmux の操作に失敗した場合は non-zero で終了します。

## Codex の Question 通知

Codex の `request_user_input_async` は、turn を止めずに質問を出します。
標準の Codex CLI 0.155.1、0.156.1、0.159.3、0.160.0 では、前述の `PostToolUse` hook でこの質問を検出できます。matcher に `request_user_input_async` を含めるか、matcher なしの `PostToolUse` hook を使ってください。
対応するのは Embedded mode（`codex --no-daemon`）だけで、subagent の質問は対象外です。

- 未確認の Question 通知があるエージェントには、サイドバーで `?` を表示します。親行の `? N` は質問数ではなく pane 数です。
- エージェントまたはその詳細行を選んで `Q` を押すと、表示中の通知を確認済みにします。`Q` は質問への回答やスキップを行いません。操作中に届いた質問は表示されたまま残ります。
- 確認済みの状態はサイドバー間で共有され、フォーカス、エージェントへの入力、run の状態、未読の Done は変わりません。
- `!`（親行では `! N`）は通知の追跡が劣化していることを示します。エージェントを展開すると理由を確認できます。

Codex が回答を受理すると、vde-tmux は質問 ID を照合し、対応する通知を自動で確認済みにします。Codex のバージョンは問いません。
一部だけ回答した場合は未回答の質問が表示されたまま残り、回答済みの項目は daemon を再起動しても保持されます。

質問をスキップしただけでは確認済みになりません。
同じ session の後続の turn で通常の prompt が受理された場合、その turn が Idle または Done になり、画面に通常の入力欄が表示されていれば、それ以前の通知を確認済みにできます。
resume、fork、`/clear`、rollback、未知の Codex バージョンや画面構成、表示の切れなどで確認できない場合は、`Q` を押すまで通知を残します。
この判定は、質問が読まれたことや回答されたことを証明するものではありません。

質問と回答の本文は保存せず、上限付きのハッシュだけを保持します。
hook を設定する前に出た質問は復元しません。
Codex のプロセスまたは pane がなくなると、その通知は閉じます。
Question 通知は status line への表示や OS 通知を追加しません。
詳細な仕様は [Question notices](./AGENT_API.md#question-notices) を参照してください。

## session とカテゴリ

カテゴリは、Repository を正規化した project の識別子ごとにまとめます。
同じ git common directory を共有する worktree は一つの Repository として扱います。

```yaml
categories:
  default_category: misc
  rules:
    - category: work
      path_patterns:
        - github.com/acme/*
```

主なコマンドは次のとおりです。

```bash
vt category next
vt category prev
vt category use work
vt category list
vt category get --repo ~/src/temporary-project
vt category create scratch
vt category assign scratch --repo ~/src/temporary-project
vt category automatic --repo ~/src/temporary-project
vt session-cycle next
vt session-cycle prev
vt session new -c ~/src/my-project
```

設定ファイルのカテゴリは vde-tmux からは変更できません。
動的カテゴリ、Repository の明示的な所属、カテゴリと Repository の並び順は、tmux socket ごとに保存します。
明示的な所属は `vt category automatic` を実行するまで設定ファイルのルールより優先され、同じ Repository の session を作り直したときにも復元されます。
All × Tree では、管理対象 session の Repository を、agent pane がない場合も表示します。
vde-tmux は外部の tmux format 向けに、有効なカテゴリを `@vde_category` へ書き出します。この option を書き換えても所属は変わりません。

`category list`、`get`、`assign`、`automatic` は `--json` を受け付けます。エージェントはこの versioned なインターフェースで Repository の所属を変更できます。
詳細は [Repository category membership](./AGENT_API.md#repository-category-membership) を参照してください。

fzf をインストールすると、session、window、pane を切り替えたり削除したりする popup を利用できます。

```bash
vt session-manager --popup
```

selector の最下段には `✕ tmux server | tmux kill-server` が表示されます。
この行を `Enter` または `Ctrl-Q` で選択すると、vde daemon の停止と残った pane プロセスの後始末を済ませてから tmux server 全体を終了します。

ghq を使っている場合は、project selector から session を作成または選択できます。

```bash
vt project selector --popup
```

## 設定ファイル

設定ファイルは `$XDG_CONFIG_HOME/vde/tmux/config.yml` に置きます。
`XDG_CONFIG_HOME` が未設定の場合は `~/.config/vde/tmux/config.yml` を使います。
すべての設定にデフォルト値があるため、設定ファイルは任意で、必要な項目だけを書けば動作します。

前節の `categories` と合わせて、よく使う設定は次のとおりです。

```yaml
sidebar:
  width: "20%"
  min_width: 40
  task_summary:
    enabled: false
    debounce_ms: 750
    timeout_ms: 90000
    # codex_model: optional-model-name
    # claude_model: optional-model-name

statusline:
  windows:
    badge_style: inline
    agent_badge:
      enabled: true
      mode: counts
      hide_idle: false
    current:
      format: " {index} {badge} "
      bold: true
      colors:
        fg: "#e8ecfb"
        bg: "#434662"
    other:
      format: " {index} {badge} "
      colors:
        fg: "#a6adc8"
        bg: "#2a2b3c"
    bell:
      fg: "#f9e2af"
    activity:
      fg: "#f9e2af"
  sessions:
    fixed_width: true
    fixed_width_alignment: center # left（デフォルト）| center
  session_badge:
    mode: rollup # rollup | counts
  summary:
    enabled: true
    hide_idle: false
    format: "{badge} {count}"

badge:
  glyphs:
    blocked: "▲"
    limited: "⋄"
    working: "●"
    done: "✓"
    idle: "○"
    unknown: "?"
```

設定全体のスキーマは `vt config schema` で確認できます。
設定を変更したら daemon を読み込み直します。

```bash
vt daemon reload
```

`statusline.summary.format` では `{badge}` と `{count}` の placeholder を使えます（`{badge}{count}`、`{badge}: {count}` など）。
件数が 0 の状態も表示するため、summary の表示幅は安定します。Idle を表示したくない場合は `hide_idle: true` を指定します。
summary を有効にしている場合は、カテゴリや window の表示が長いときも常に表示します。
Window マップの先頭にある `W N` は、現在の Session に属する Window の総数です。
各セルは tmux の実際の index を使い、0 始まりや欠番もそのまま表示します。現在の index は反転表示します。
現在の Window 名だけをマップの後ろに表示し、最大16セルに制限します。
Agent の件数は Window ごとに集計し、Session バッジと同じ順序・色で、Blocked、Limited、Working、未読 Done、Unknown、Idle を表示します。件数表示はデフォルトで有効で、0 件の状態は省略します。
マップには独立した80セルの表示幅を割り当てます。収まらない場合は現在の Window と連続する近傍を残し、省略した Window 数を左右それぞれに表示します。
省略した側にある Blocked、Limited、未読 Done の Agent 件数も残します。
縮小表示でも現在の index と Agent 件数を維持します。内部の `@` ID はクリック対象の識別にだけ使います。
各セルは最も広いセルに合わせて余白を補い、現在の Window を切り替えたときの位置のずれを抑えます。
bell（`♪`）と activity（`·`）は Agent バッジと別の印で表示し、Window 全体の色は変更しません。
既存の設定で `{index}:{window}` を使っている場合や `windows.agent_badge` が無効の場合は、上記のマップ用設定へ更新してください。
カテゴリの表示には、session があるすべてのカテゴリを、status の幅を超える場合も省略せずに表示します。

`statusline.sessions.fixed_width: true` を指定すると、session の表示領域を最も広いカテゴリに合わせ、session を切り替えてもカテゴリ、session、window を合わせた領域の幅を一定に保ちます。
デフォルトは左寄せで、中央寄せにする場合は `fixed_width_alignment: center` を指定します。
session の `current` と `other` の format で表示幅が異なる場合は、数セルの差が生じることがあります。

`sidebar.task_summary.enabled` を有効にすると、エージェントの行に現在の task の短い要約を表示します。
daemon はエージェントに対応する CLI（Codex は `codex exec`、Claude は `claude -p`）で要約を非同期に生成し、別の provider へ切り替えることはありません。
要約のために、上限付きで可能な範囲の秘匿処理をした prompt を追加の model request として送信します。これを許容できない場合は無効のままにしてください。
model 名は任意で、指定しない場合はインストール済み CLI のデフォルトを使います。
要約は現在のエージェントの直近4件の prompt に追従し、新しい要約の生成待ちや生成失敗の間は古い要約を表示しません。
`vt agent get --json` は `task_summary_status`（`current` または `failed`）と、失敗時の `task_summary_error` を返します。

### Codex の容量エラー

vde-tmux は、`--no-daemon` で起動した Codex CLI 0.160.0 の turn が `Selected model is at capacity. Please try a different model.` で失敗したとき、作業の再開を依頼できます。
容量エラーは設定に関係なく Error / Unresolved として記録し、Done には数えません。
自動再開は初期状態で無効です。次のように有効にします。

```yaml
codex:
  capacity_auto_resume:
    enabled: true
    # prompt を省略すると英語のデフォルト文面を使います。UTF-8 で変更できます。
    prompt: |-
      Resume only unfinished work after checking the current state. Preserve the original objective, scope, constraints, approvals, and response language. Do not repeat completed operations. This message is not a new approval.
```

同じ model に対して、60、120、300秒後（0〜20% の揺らぎを加える）に、一連の失敗につき最大3回まで再開を依頼します。
model の変更や新たな承認は行いません。

再開の依頼を送るのは、pane が同じ失敗した turn を表示したまま入力欄が空で、失敗後に何も変化していない場合だけです。queued input、画像、dialog、Question 通知、copy mode、その pane での操作のいずれかがあれば送りません。
手動の prompt、session やプロセスの変更、正常な完了、別のエラーで一連の再開は終了します。
再開の対象になるのは、現在の daemon が動いている間に人が送った prompt だけです。
他の Codex バージョン、狭い pane、変化した画面でも失敗は記録しますが、再開はしません。
待機中の再開は daemon の再起動で破棄します。
再開の依頼が届いたか確認できない場合は、Codex のプロセスまたは pane が置き換わるまで、daemon を再起動しても送信を止めたままにします。

標準の Codex では、入力欄の確認と送信を一度に行えません。最後の確認から貼り付けまでの間に、手入力や承認画面が割り込む可能性は残ります。
この制約を許容できる場合に有効にしてください。

`vt daemon diagnostics --json` の `codex_capacity_auto_resume` で、試行回数、次回の再開予定、停止理由を確認できます。prompt の本文は含みません。
独自の prompt には LF を含められ、末尾の LF を除いて 65,536 UTF-8 bytes 以下にします。
空の文面、前後の空白、危険な制御文字、先頭の `/`・`!`、末尾の `@`・`$` の補完 token があると、daemon は起動や reload を拒否します。

## 通知

エージェントが `Blocked` へ移ったときに外部コマンドを実行できます。

```yaml
notify:
  enabled: true
  command: 'terminal-notifier -title vde-tmux -message "$VDE_AGENT needs attention"'
```

通知コマンドには `VDE_PANE_ID`、`VDE_AGENT`、`VDE_BADGE_STATE` が渡されます。
実行待ちの通知は、実行直前に Blocked が解消済みであれば実行しません。

## エージェント向け JSON API

エージェントは tmux をポーリングせずに、pane や他のエージェントの状態を確認し、完了を待てます。

```bash
vt api schema --json
vt api snapshot --json
vt agent list --status working --json
vt agent wait %456 --until done,blocked,limited --json
vt pane read %456 --source latest --lines 120 --json

AGENT_REF="$(vt agent get %456 --json | jq -r '.result.agent.summary.agent_ref')"
REQUEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/vt-request.XXXXXX")"
printf '%s' '現在の差分をレビューしてください。' >"$REQUEST_DIR/prompt.txt"
PROMPT_JSON="$(vt agent request "$AGENT_REF" \
  --state-file "$REQUEST_DIR/request.json" \
  --prompt-file "$REQUEST_DIR/prompt.txt" --json)"
RUN_REF="$(printf '%s' "$PROMPT_JSON" | jq -r '.result.run_ref')"
vt agent run wait "$RUN_REF" --json
vt agent run response "$RUN_REF" --json
```

- `vt api snapshot --json` は、pane、エージェント、daemon の診断情報を一貫した一つの revision で返します。`tmux list-panes`、`vt pane list`、`vt agent list` を組み合わせるより、こちらを使ってください。
- `vt agent request` は、Codex のエージェントへ prompt を確実に送ります。進行状況は `--state-file` に指定したファイルへ保存します。応答を受け取れなかった場合は、同じ対象と state file で prompt の本文を付けずに再実行すると、再送せずに続きから確認できます。新しい prompt ごとに新しい state file の path を使ってください。prompt の本文が argv に現れることはありません。
- `--no-daemon` で起動した直後の Codex session は、Unknown と表示されている間でも最初の `agent request` を受け付けます。
- `agent send`（idle または done のエージェント）、`agent steer`（作業中のエージェント）、`agent send-keys`（Blocked のエージェント）で、保護された端末入力を送れます。`pane split` と `agent start` で pane の作成とエージェントの起動ができます。
- 変更操作には exact な参照が必要です。`agent_ref` は、PID と起動時刻で生きているエージェントのプロセスを一つに特定できる間だけ発行されます。

完全な仕様は [Agent JSON API](./AGENT_API.md) を参照してください。

## その他のエージェントを接続する

Claude Code と Codex 以外のエージェントは、`vt hook emit` で状態を送れます。
`--session-id` には一つのエージェント実行中に変わらない ID を指定します。

```bash
vt hook emit \
  --agent myagent \
  --session-id run-42 \
  --status running \
  --prompt "fix the build" \
  --prompt-source user
```

`--status` は `running`、`waiting`、`idle`、`error` を受け取ります。
`--prompt` は表示用の情報としてプロセスの argv に渡るため、秘密情報には使わないでください。
入力待ちを送る場合は理由も指定します。

```bash
vt hook emit \
  --agent myagent \
  --session-id run-42 \
  --status waiting \
  --wait-reason permission_prompt
```

利用上限に達したことは `--wait-reason usage_limit` で送れます。

## daemon の操作

通常は tmux 設定の `vt daemon ensure` だけで起動を管理できます。

| コマンド | 用途 |
| --- | --- |
| `vt daemon ensure` | daemon が必要なら起動する |
| `vt daemon reload` | 設定を検証して再起動する |
| `vt daemon stop` | daemon を一時停止する |
| `vt daemon disable` | 自動起動を無効にして停止する |
| `vt daemon enable` | 自動起動を有効にして起動する |
| `vt daemon status` | daemon と hook の状態を表示する |

`stop` は自動起動を無効にしません。
停止状態を維持したい場合は `disable` を使います。

### pane state の永続化

daemon は、prompt、task、subagent、状態遷移、要約、既読状態などの pane の詳細を、tmux server ごとに `$XDG_STATE_HOME/vde-tmux/<incarnation-hash>/pane-state-v11.json` へ保存します。
daemon の再起動後は、pane ID と PID が一致する pane についてこれらを復元します。
すでに動いていない tmux server のファイルは自動では削除しません。

ファイルが破損している、または権限が安全でない場合、daemon は起動せず、`vt daemon status` の `last_transition_error` にファイルの path を表示します。
その tmux server の保存済み pane state をすべてリセットする場合に限り、そのファイルを削除してから `vt daemon ensure` を実行してください。

## アップグレード

daemon とそのクライアント（サイドバー、status line、CLI）はバージョンが一致している必要があり、異なるバージョン間の互換はありません。
バイナリを差し替える前に daemon を止め、新しい daemon を起動してからサイドバーを開き直します。

```bash
vt daemon stop
cargo install vde-tmux --locked
vt daemon ensure
```

古い daemon が動いたままバイナリを差し替えた場合は、`vt daemon stop --force` で停止できます。
pane state の schema が変わるアップグレードでは、保存済みの pane の詳細は移行されずにリセットされます。

## トラブルシュート

### status line またはサイドバーが更新されない

daemon の状態を確認します。設定を変更した場合は読み込み直します。`reload` は設定を検証し、エラーがあれば表示します。

```bash
vt daemon status
vt daemon reload
```

通知、status の更新、hook の配送のエラーは `$XDG_STATE_HOME/vde-tmux/<incarnation-hash>/daemon.log` に記録されます。

### tmux の設定を読み込むと hook が壊れる

vde-tmux は tmux hook の index `70` を使います。
同じ hook に独自の処理を追加する場合は、別の index を明示してください。

```tmux
set-hook -g client-session-changed[0] 'your-command'
```

index を付けない `set-hook` は既存の hook 配列を置き換えます。

## 既知の制約

- hook がない場合、入力待ちの判定は pane に表示された内容から推測できる範囲に限られる
- daemon が停止すると最後に描画した status option が残り、次の hook event または `vt daemon ensure` まで更新されない
- path に空白を含む Codex の実行ファイルやスクリプトは識別できない場合があり、そのときは hook の所有者が未検証として報告される

## License

[MIT](./LICENSE)
