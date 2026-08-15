# vde-tmux

[English](./README.md) | **日本語**

vde-tmux は、tmux で動かしている AI コーディングエージェントの状態を一覧できるツールです。
Claude Code、Codex、opencode の pane を追跡し、tmux の status line とサイドバーへ状態を表示します。

![vde-tmux sidebar](https://github.com/user-attachments/assets/e912448f-b657-49d9-b175-39a0cbad04f2)

## できること

- すべての tmux session にいるエージェントを `Blocked`、`Working`、`Done`、`Idle` に分類する
- 対応が必要なエージェントを status line に表示する
- prompt、経過時間、task、subagent、worktree activity をサイドバーに表示する
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
set -g status-left '#{@vde_status_category}#[fg=#8f8ba8] │ #[default]#{@vde_status_sessions}#[fg=#8f8ba8] │ #[default]#{@vde_status_windows}'
set -g status-right '#{@vde_status_attention} #{@vde_status_summary}'

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
- daemon は実際に起動している `vt` の絶対パスを `@vde_executable` へ保存します。Neovim pane navigation は PATH を検索せず、この実体を使います。
- vde-tmux は描画済みのテキストを `@vde_status_*` option へ書き込むため、status line の再描画ごとに外部プロセスは起動しません。
- `@vde_status_now_format` は pane border の経過時間表示に必要です。
- `Blocked`、`Working`、`Done` の agent pane は、pane statusline の残り幅をバッジと同じ色の一重罫線で埋めます。`pane-border-status bottom` のため下辺だけが強調され、左右の本文セルには重なりません。`Idle` と non-agent pane には追加の罫線を描きません。
- `window-status-*` の設定は、tmux 標準の window list を vde-tmux の session と window の表示へ置き換えます。
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

デフォルトの `<C-h/j/k/l>` は Neovim 内では window 間を移動し、端ではtmux内の軽量なsignalをdaemonへ送り、tmux paneへ移動します。tmux root bindingはキーごとの`vt`/`tmux` processを起動せず、Neovimの端からは1つのtmux clientだけでsignalを送ります。移動先が Neovim の場合は、移動元のカーソル座標に合う window を選択します。選択情報は移動先 pane の option に PID とともに保存されるため、別 client や再利用された pane が誤って消費しません。

pane移動はdaemonが所有する1つの常駐tmux control-mode clientを通して実行します。このclientは`ignore-size`、`no-output`、`no-detach-on-destroy`付きで既存sessionへattachし、vde-tmuxがregular clientのattach有無を判定するときは除外されます。nativeな`tmux list-clients`には表示されます。またregular clientが1つもいない間、control clientのattach先となる1 sessionでは、tmux nativeのalertと`destroy-unattached`判定がtmuxのattached-client意味論に従います。

`require('vde-tmux').navigate('h')` のように API だけを既存の mapping から呼ぶこともできます。`setup()` には `keybindings = false`、`modes`、`debug`、`disable_when_floating`、`navigate_from_floating` を指定できます。

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
    "Stop": [{ "hooks": [{ "type": "command", "command": "vt hook claude Stop" }] }]
  }
}
```

保存後に Claude Code を再起動すると、状態遷移と task の進捗が表示されます。

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
        "matcher": "^update_plan$",
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

Codex を再起動すると、permission request、plan、subagent、worktree activity がサイドバーへ反映されます。

### 5. 動作確認

tmux 内で次のコマンドを実行します。

```bash
vt daemon status
vt sidebar open
```

hook を設定していなくても、Claude Code、Codex、opencode は pane の実行コマンドから検出できます。
ただし、prompt、完了時刻、入力待ちを正確に表示するには hook が必要です。

## エージェント向け JSON API

エージェントは tmux topology をポーリングせず、daemon の canonical topology cache を参照し、
実プロセスで識別された同一の agent occupant を固定して完了を待てます。

```bash
vt api schema --json
vt agent list --status working --json
vt agent wait %456 --until done,blocked --json
vt pane read %456 --source latest --lines 120 --json

AGENT_REF="$(vt agent get %456 --json | jq -r '.result.agent.summary.agent_ref')"
printf '%s' '現在の差分をレビューしてください。' | vt agent prompt "$AGENT_REF" --stdin --json
```

response envelope、occupant を固定する参照、filter、capture 上限については
[Agent JSON API](./AGENT_API.md) を参照してください。PID と OS の process start token で一意な
実プロセスを固定できる場合だけ exact `agent_ref` が発行されます。正確な lifecycle 表示には
引き続き hook が必要ですが、実プロセスを一意に識別できれば hookless agent でも
`agent wait` / `agent read` を利用できます。guarded prompt dispatch はさらに、daemon 管理下の
tmux hook が healthy であること、Claude Code / Codex 向けの prompt adapter があること、対象が
idle/done かつ foreground input owner であることを要求します。外部 provider hook は事前の
health 値ではなく、送信後の digest event によって確認します。成功時は digest 確認済み receipt
を返し、配送が曖昧な場合は自動再送しません。

## 状態の読み方

| 表示 | 状態 | 意味 |
| --- | --- | --- |
| `▲` | Blocked | 許可や回答など、利用者の入力を待っている |
| `●` | Working | エージェントが作業している |
| `✓` | Done | 作業が完了し、まだ確認されていない |
| `○` | Idle | 作業がない、または完了を確認済み |

`Done` は、対象の exact pane が eligible な tmux client で active になると `Idle` になります。
同じ window の別 split を見ても既読にはなりません。
既読状態は daemon の再起動後も保持され、すべての tmux client とサイドバーで共有されます。
daemon は現在の client view を定期的に再照合するため、view hook を一度取りこぼしても次の
observation pollで修復されます。

`unread-latest` は全paneを横断し、未読のWaiting、Error、Completedのうち最新の発生へ移動します。
globalな発生順はdaemonが管理し、移動中に最新paneが消えた場合は次の未読paneを試します。
移動操作そのものは既読化せず、移動先がactive paneとして観測された後に既読になります。

## サイドバー

サイドバーは現在のtmux windowに開き、対象範囲と表示方法を独立した2軸で切り替えます。
`Current`はそのサイドバーの起点sessionが属するcategoryだけ、`All`は全categoryを対象にします。
`Tree`はCurrentではRepository→Agent、AllではCategory→Repository→Agentの階層表示です。
`Priority`は選択中のscopeをPinned、Needs Input、Unread Done、Running、Idleの順にまとめ、
`Flat`はgroupingを外します。
Priorityで未読agentを選択して`p`を押すと、共有されたUnread Spanのpinを切り替えます。
pinされたagentは先頭の`PINNED` zoneへ移動しますが、未読順、badge、notificationは変わりません。
exact paneを表示した場合は通常どおり既読になり、pinも解除されます。

```bash
vt sidebar open --width 40
vt sidebar open --width 20%
vt sidebar toggle
vt sidebar toggle --all
vt sidebar rail
vt sidebar close
```

`vt sidebar focus-toggle` は、サイドバーがなければ開き、表示中ならフォーカスし、フォーカス中なら閉じます。

| キー | 動作 |
| --- | --- |
| `j` / `k`、`↓` / `↑` | 行を移動する |
| `gg` / `G` | 先頭行または末尾行へ移動する |
| `Ctrl-D` / `Ctrl-U` | 半ページ下または上へ移動する |
| `Ctrl-F` / `Ctrl-B` | 1ページ下または上へ移動する |
| `Enter` | 選択したエージェントの pane へ移動する |
| `Space` | 選択行を開閉する |
| `c` | category scopeをCurrent / Allで切り替える |
| `v` | presentationをTree / Priority / Flatの順に切り替える |
| `1` / `2` / `3` | Tree / Priority / Flatへ直接切り替える |
| `Tab` / `Shift+Tab` | 状態フィルタを切り替える |
| `n` / `N` | 次または前の要対応エージェントへ移動する |
| `p` | Priorityで選択中の未読agentをpinまたはunpinする |
| `d` | 選択中の run を完了としてマークする |
| `J` / `K` | 手動順序を変更する |
| `q` / `Esc` | サイドバーを閉じる |

現在の session に属するエージェントには左端へ `▎` を表示します。
エージェントの1行目をクリックすると開閉を切り替えます。2行目以降をクリックすると、
事前に選択していなくてもそのエージェントのpaneへ移動します。
キーボードでは選択中のエージェントを`Space`で開閉します。
マウスホイールは選択カーソルを動かさず、はみ出した表示範囲をスクロールします。
起動後にまだ操作されていないエージェントは、閉じている間は1行だけ表示します。
category scope、presentation、filter、手動順序、開閉状態、選択位置、スクロールは、
同じtmux serverの全サイドバーで同期します。具体的なCurrent categoryとreturn targetだけは
sidebar instanceごとに保持し、そのサイドバーへ入力した起点sessionへ追従します。
開いているサイドバーへ非focus状態でpin操作を送る場合は`pin-toggle`を使えます。

```tmux
bind-key -n M-p run-shell "vt sidebar input pin-toggle --window #{q:window_id}"
```

## session とカテゴリ

カテゴリを使うと、canonicalなRepository identity単位でtmux sessionをまとめられます。
同じgit common directoryを共有するworktreeは一つのRepositoryとして扱われます。

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
vt category create scratch
vt category assign scratch --repo ~/src/temporary-project
vt category automatic --repo ~/src/temporary-project
vt session-cycle next
vt session-cycle prev
vt session new -c ~/src/my-project
```

configのカテゴリはread-onlyな最低限の定義として残ります。
動的カテゴリ、Repositoryの明示的な所属、カテゴリとRepositoryの順序はtmux socketごとに保存されます。
明示的な所属は`vt category automatic`を実行するまでconfig ruleより優先され、同じRepositoryのsessionを作り直した場合も復元されます。
サイドバーでは`a`でカテゴリ追加、`m`でRepository移動、`r`で動的カテゴリ名変更、`D`で削除、`J`/`K`でカテゴリまたはRepositoryを並べ替えられます。
All × Treeでは、管理対象sessionのRepositoryはagent paneがない場合も表示されます。
`@vde_category`は外部tmux format向けの導出済みwrite-only mirrorです。

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

statusline:
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
    working: "●"
    done: "✓"
    idle: "○"
```

`statusline.summary.format` では `{badge}` と `{count}` の placeholder を使えます（`{badge}{count}`、`{badge}: {count}` など）。
件数が 0 の状態も表示するため、summary の表示幅は安定します。
Idle を表示したくない場合は `hide_idle: true` を指定します。

category segmentでは、agent paneが0件の場合も、sessionを持つすべてのカテゴリを表示します。
各カテゴリは、共有status幅のbudgetを超える場合も完全なラベルと操作targetをpublishし、`+N`や`cat:N`へ省略しません。

`statusline.sessions.fixed_width: true` を指定すると、active category の session 領域を最も広い category に合わせ、category、session、window を合わせた領域も全 session で同じ幅に揃えます。
固定領域内のsession表示はデフォルトで左寄せです。中央寄せにする場合は `fixed_width_alignment: center` を指定します。
window 名やプロセス名の長さが異なる session を切り替えても、中央寄せした status block の位置がずれません。
inactive category の幅には session の `other` style を使うため、`current.format` と `other.format` の表示幅が異なる場合は数セルの差が生じることがあります。

設定全体のスキーマは `vt config schema` で確認できます。

設定を変更したら daemon を読み込み直します。

```bash
vt daemon reload
```

## 通知

エージェントが `Blocked` へ移ったときに外部コマンドを実行できます。

```yaml
notify:
  enabled: true
  command: 'terminal-notifier -title vde-tmux -message "$VDE_AGENT needs attention"'
```

通知コマンドには `VDE_PANE_ID`、`VDE_AGENT`、`VDE_BADGE_STATE` が渡されます。

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
入力待ちを送る場合は理由も指定します。

```bash
vt hook emit \
  --agent myagent \
  --session-id run-42 \
  --status waiting \
  --wait-reason permission_prompt
```

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

daemon は tmux server incarnation ごとに一つの private な full-state snapshot を
`$XDG_STATE_HOME/vde-tmux/<incarnation-hash>/pane-state-v8.json` へ保存します。
daemon 再起動後も、pane ID と PID が一致する pane の prompt、task の進捗と項目、subagent、worktree activity、lifecycle、時刻、agent identity、Unread Spanのpin、Done と確認済み状態を復元します。

snapshot が破損している、または権限が安全でない場合、daemon は修復や fallback を行わず起動を停止します。
`vt daemon status` の `last_transition_error` に snapshot path が表示されます。
その tmux server の保存済み pane state をすべてリセットする場合に限り、表示された file を削除してから `vt daemon ensure` を実行してください。

productionの起動処理は、古いpane-state schemaのsnapshotを移行しません。
別途ワンショット移行を行わない場合、schema更新後にpane詳細はリセットされます。
別のtmux server incarnationのsnapshotは自動削除しません。

## アップグレード

daemon とそのクライアント（サイドバー、status line、CLI）はバージョンが一致している必要があり、異なるバージョン間の互換はありません。
バイナリを差し替える前に daemon を止め、新しい daemon を起動してからサイドバーを開き直します。

```bash
vt daemon stop
cargo install vde-tmux --locked
vt daemon ensure
```

古い daemon が動いたままバイナリを差し替えた場合は、`vt daemon stop --force` で停止できます。

## トラブルシュート

### status line またはサイドバーが更新されない

daemon の状態を確認し、設定を変更した直後であれば読み込み直します。

```bash
vt daemon status
vt daemon reload
```

### tmux の設定を読み込むと hook が壊れる

vde-tmux は tmux hook の index `70` を使います。
同じ hook に独自の処理を追加する場合は、別の index を明示してください。

```tmux
set-hook -g client-session-changed[0] 'your-command'
```

index を付けない `set-hook` は既存の hook 配列を置き換えます。

### 設定エラーを確認する

```bash
vt daemon reload
vt daemon status
```

tmux server incarnation ごとの運用 log は
`$XDG_STATE_HOME/vde-tmux/<incarnation-hash>/daemon.log` 一つです。
notification、status push、hook delivery の error は、この file 内でそれぞれ異なる prefix を使います。
サイドバーの並び順、category scope、presentation、filter、行の展開状態は、tmux socketごとに分離された
`$XDG_STATE_HOME/vde/tmux/sidebar-state/` 配下の一つのファイルへatomicに保存されます。
同じtmux serverのサイドバー間では保存対象の値と選択、スクロールが即時共有されます。
選択とスクロールは daemon の稼働中だけ共有され、保存はされません。
具体的なCurrent categoryとreturn targetはinstance localのまま保存されません。

## 既知の制約

- hook がない場合、入力待ちの判定は pane に表示された内容から推測できる範囲に限られる
- daemon が停止すると最後に描画した status option が残り、次の hook event または `vt daemon ensure` まで更新されない

## License

[MIT](./LICENSE)
