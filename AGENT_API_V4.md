# Agent API v4

## Status

実装・release gate・local cutover完了。API v3を置換し、runtimeを併存させない。daemon protocol 16、PaneState 9、private state format 1は変更しない。

## Goal

Agent API v4は、既存のcanonical topology、durable Codex dispatch、Run/Operation/Response Artifactに加え、agent bridgeがraw tmux入力を使わずにpane分割、agent起動、guarded terminal prompt、実行中agentへのbest-effort steer、blocked promptへのlogical key入力を行えるJSON契約を提供する。

## Scope

v4で追加する公開契約は次のとおりとする。

- agent kindをkeyにしたprovider capability contract
- exact `pane_ref`を起点にする`pane split`
- exact shell paneへproviderを起動し、exact readinessまで待つ`agent start`
- exact idle/done agentへguarded terminal入力する`agent send`
- exact working Codex/Claudeへbest-effort入力する`agent steer`
- exact blocked agentへallowlisted logical keyを入力する`agent send-keys`
- mutation receiptから同じoccupantのlifecycle cursorへ接続する運用契約

次は非対応とする。

- session、window、worktree、categoryの作成や削除
- geometryの完全なtmux互換API、relative selector、alias target
- agent kill、respawn、自動reconciliation
- global event stream、wait-any
- providerの受理を確認できないtransportから別transportへのfallback
- Claude Codeの未検証durable adapter

## Provider capability contract

`vt api schema --json`の`result.contract.providers`はagent kindを直接keyにする。callerは独自のprovider名変換表を持たず、resolved agentのkindで参照する。

| agent kind | prompt dispatch | steer | confirmation | response | logical keys | start readiness |
| --- | --- | --- | --- | --- | --- | --- |
| `codex` | `durable` | `guarded_terminal_best_effort` | `provider_digest` | `artifact` | yes | `durable_initial_prompt` |
| `claude` | `guarded_terminal` | `guarded_terminal_best_effort` | `lifecycle_cursor` | `terminal_read` | yes | `provider_session` |
| `opencode` | `guarded_terminal` | `disabled` | `none` | `terminal_read` | yes | `input_owner_only` |

`prompt_confirmation=none`はmutation primitiveの存在だけを示し、自動agent bridgeの受理契約を満たさない。bridgeは送信せず停止する。

## Mutation contract

すべてのmutationはraw pane IDではなくexact public referenceを要求する。server incarnation、pane ID/PID、agent process identity、foreground input ownerを必要なfenceで再検証し、copy-mode中の入力は同じguarded tmux command内でcopy-mode解除後に再検証してから適用する。

prompt bodyはstdinまたはfileから読み、argv、receipt、canonical snapshotへ格納しない。`agent send`成功はtmux input適用を示すだけでprovider受理を示さない。callerはreceiptの`baseline_completed_seq`とexact `agent_ref`を`agent wait`へ渡し、新しい`working`、`blocked`、またはcursorより新しい`done`を確認する。

`agent steer`は開始snapshotで`working`なexact Codex/Claudeだけを受け付け、同じguarded terminal mutationを適用する。active turnへの帰属確認やhook待ちは行わない。入力と同時にturnが完了した場合は次turnとして開始され得るため、receiptは`race_policy=may_start_next_turn`を返す。成功はtmux input適用だけを示し、現在turnへの割り込みやprovider受理を保証しない。

`pane split`は既定でfocusを移さず、cwdを起点paneから継承する。成功は作成paneのlive identityとdaemon canonical topologyへの反映を含む。

`agent start`は`codex`、`claude`、`opencode`の固定program mapだけを受け付ける。shell control文字を含む引数を拒否し、promptを起動引数として扱わない。全providerでexact agent processとforeground input ownershipを待つ。Claude Codeはさらにprovider session観測後のprocess再検証まで待つ。Codexはsession未確定でもdurable first-promptが確定できる契約、opencodeはprovider受理を保証しない`input_owner_only`としてreceiptとschemaへ明示する。

`agent send-keys`はblocked agentだけを対象にし、一文字またはclosed allowlistのlogical keyを最大16個受け付ける。prompt文字列をkey列へ分解する用途には使わない。

side effect開始後のtimeout、接続断、marker欠落は`side_effect=possible|confirmed`と`retry_action=inspect_manually`で返す。callerはsplit、start、prompt、keyを自動再実行しない。

## Version contract

| contract | version |
| --- | ---: |
| public Agent API | 4 |
| daemon protocol | 16 |
| PaneState | 9 |
| private state format | 1 |

API v3をruntimeで受け付けるcompatibility modeやraw tmux fallbackは設けない。

## Definition of Done

### 機能完了条件

- [x] schemaがagent kindごとのdispatch、confirmation、response、logical key、start capabilityを公開する。
- [x] exact pane splitがcwd、方向、比率、focus policyをreceiptへ返し、canonical projection後だけ成功する。
- [x] agent startが固定providerをexact shell paneで起動し、provider別readinessとinput ownershipを返す。
- [x] guarded terminal sendがcopy-mode解除、identity/input-owner fence、private body入力を一つのmutationとして行う。
- [x] best-effort steerがworking gate、provider capability、copy-mode解除、exact occupant/input-owner fenceとnext-turn race policyを公開する。
- [x] blocked agentへのlogical key入力がclosed validationと同じguardを使う。
- [x] bridge skillがcapabilityからtransportを選び、raw tmuxへfallbackしない。

### テスト完了条件

- [x] terminal mutationのargv非露出、copy-mode、logical key、detached splitをunit testで検証する。
- [x] isolated tmuxでsplit、start、copy-mode中send/steer、steer status gate、blocked send-keys、schema contractを検証する。
- [x] `cargo fmt --check`、warnings denied clippy、full test、ignored testがpassする。
- [x] runtime smoke、UI/UX preflight、kill-server isolated testがpassする。
- [x] durable prompt、operation crashのisolated testがpassする。

### 運用反映条件

- [x] private storeにin-flight operationがないことをcutover前に確認する。
- [x] daemon停止後にlocal binaryを差し替え、新daemonを起動する。
- [x] installed `vt api schema`がAPI 4 / protocol 16 / PaneState 9 / private state 1を返す。
- [x] dotfiles管理下の`tmux-agent-bridge`がAPI v4 gateと同じprovider capability contractを使う。
- [x] bridgeが`steer` capabilityとnext-turn race policyを検査し、通常sendやraw tmux入力へfallbackしない。
