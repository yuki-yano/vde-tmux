# Agent API v3

## Status

履歴仕様。API v4で置換済みであり、runtimeを併存させない。v4の追加契約とrollout gateは[AGENT_API_V4.md](AGENT_API_V4.md)を参照する。

API v2はv3 cutoverまでの公開契約であり、v3はv4 cutoverまでの公開契約だった。

Codex 0.147.0のprovider境界は実測済みでadapterを有効化した。Claude Codeはauthenticated isolated P0を通過していないため、durable mutationを有効化しない。

## Goal

Agent API v3は、prompt送信、run完了待機、応答回収、状態driftの診断と復旧を、daemon再起動とagent交代をまたいで安全に扱えるJSON契約を提供する。

## Scope

API v3に含める機能は次のとおりとする。

- Agent Occupant、Agent Run、Dispatch Operationを別のidentityとして公開する。
- Run EvidenceとRun Resolutionを分離する。
- prompt dispatchをdaemon-ownedのdurable operationとして実行する。
- run単位のbounded Response Artifactをcanonical Pane snapshotの外へ保存する。
- read-only診断とoperator-authorized completion recoveryを提供する。
- provider hookの観測状況を、設定済み保証ではなく観測事実として公開する。

API v3に含めない機能は次のとおりとする。

- global event stream
- pane geometryとrelative pane selector
- agent spawn
- wait-any
- vde-monitor向けraw hook payload bus
- opencode向けraw tmux fallback
- completionを変更するautomatic reconciliation
- failedまたはcancelledへのoperator resolution
- 汎用provider adapter framework

これらのうちpane split、agent start、provider capability契約、guarded terminal mutationはAPI v4で追加した。残りは引き続き非対応とする。

## Design principles

- correctnessに必要な永続化はRun、Operation、body blobへ限定する。
- provider hookを無損失WALとして扱わない。hook処理中のcrashでcompletionを失った場合は、unresolved runと明示的なrecovery経路で回収する。
- dispatchのat-most-onceは弱めない。`dispatch_started`のdurable化後は自動再送しない。
- process absence、terminal静止、ready表示をsemantic completionへ昇格させない。
- canonical Pane snapshotへ履歴、guarded dispatch prompt、full response、event ledgerを埋め込まない。手入力promptとresponseはsidebar表示に必要なbounded one-line previewだけを保持する。
- private stateは同じstate format versionで一体管理し、storeごとの公開versionを増やさない。
- runtime fallbackとv2/v3併存を設けない。

## Current constraints

API v2の`agent_ref`はtmux server、Pane ID、Pane PID、pane-state ID、Agent Epoch、agent PID、process start tokenを固定する。

API v2の`agent prompt`はCLI processがper-pane lock、tmux input、provider digest確認を実行するため、CLI終了後のside effectをdaemonが再照合できない。

API v2の`completed_seq`はprovider completion、process absence、terminal stale inference、sidebar MarkDoneを同じ完了として表現する。

現行`MarkDone`はclosed runにもsynthetic runを作り、tasks、subagents、worktree activity、unread projectionを変更するため、operator recoveryへ流用しない。

現行`latest_response`は一行のbounded previewであり、長いreview responseの回収契約には使えない。

## Domain relationships

一つのAgent Occupantは、一つのAgent Epoch内で複数のAgent Runを順番に実行する。

一つのAgent Runは、人間が直接入力したpromptから始まる場合と、一つのDispatch Operationから始まる場合がある。

一つのDispatch Operationは、最大一つのAgent Runへ結び付く。

一つのAgent Runは複数のRun Evidenceを持ち、最大一つのRun ResolutionとResponse Artifactを持つ。

Dispatch Operationが`delivery_unknown`になっても、対応するAgent Runが後からprovider hookで確認される場合がある。

## Public identities

### Agent binding

mutationの内部preconditionには、公開referenceとは別に完全なAgent Bindingを使う。

RunのAgent Bindingはtmux server identity、Pane instance、pane-state ID、Agent Epoch、agent kind、provider session ID、agent PID、process start tokenを固定する。

Dispatch OperationのOperation Bindingは同じidentityを使うが、SessionStartが未観測のCodexに限りprovider session IDを未確定でstageできる。guarded dispatch前にexact process identityとinput ownerを再検証し、同じpane-state ID、agent kind、process identity、prompt digestを持つ最初の`UserPromptSubmit`だけで実sessionへ一度だけ確定する。SessionStartがdispatchと競合した場合は、同じprocessに対するAgent Epochの1増加だけを許可する。

process scanだけで発見したoccupantもguarded prompt dispatchのpreconditionを発行できるが、未確定Operation BindingをRunへ保存せず、provider hookでsessionを確認できなければ`delivery_unknown`のまま保持する。operator completionのpreconditionには引き続き完全なRun Agent Bindingを要求する。

### Agent reference

`agent_ref`はAgent Occupantを固定するopaque referenceとする。

exact process identityを取得できないoccupantには発行しない。mutation時にはAgent Binding全体を再検証する。

### Run reference

`run_ref`はRun作成時にdaemonが発行するrandom stable run ID、tmux server identity、state generationを固定するopaque referenceとする。

`run_seq`は一つのAgent Epoch内の表示順であり、Run identity、completion判定、wait cursorには使わない。

Run Recordは作成時のAgent Binding、optional provider-native turn key、`run_seq`、optional `operation_ref`を保持する。

手入力で開始したAgent Runにも`run_ref`を発行する。

Agent Epochまたはprocess identityが変わった場合、古い`run_ref`を新しいoccupantへ再束縛しない。

### Operation reference

`operation_ref`はcallerが指定した`operation_id`、tmux server identity、state generationから作るopaque referenceとする。

同じ`operation_id`を異なるtargetまたはprompt digestで再利用した場合は`operation_conflict`として拒否する。

state resetによってgenerationが変わった場合、以前の`operation_ref`は`operation_generation_replaced`となる。

## Agent Run state

### Usage-limit lifecycle

Claude CodeまたはCodexが利用上限へ到達したPaneは、occupantの公開statusを`blocked`、lifecycleを`waiting`、reasonを`usage_limit`として投影する。

Claude Codeの`StopFailure(error_type=rate_limit)`を一次情報とする。Codexには同等のfailure hookがないため、daemonは実行中のClaude Code/Codex paneだけを5秒間隔で一括captureし、直近30行の行頭にある`You've hit your session limit`または`You've hit your usage limit`を補助証拠として扱う。通常のrate-limit文、statuslineの残量警告、古いscrollbackは状態を変更しない。

利用上限はsemantic completionではない。processが終了した場合もopen runと`usage_limit`を保持し、`present=false`のAgent Summaryとして参照できる。時刻到達だけではproviderの回復を証明できないため自動retryや時計ベースの解除は行わず、後続の`SessionStart`または`UserPromptSubmit`を回復証拠とする。reset時刻を必要とするcallerは`vt pane read`でprovider原文を取得する。

Runはexecutionとsemantic outcomeを別fieldとして持つ。

`execution_phase`は`running`、`waiting`、`error`、`ended`のいずれかとする。

`semantic_outcome`は`unresolved`または`completed`とする。

`resolution`は`none`、`provider_completed`、`operator_completed`のいずれかとする。

`ended_unconfirmed`は`execution_phase=ended`かつ`semantic_outcome=unresolved`のpublic projectionである。

`semantic_outcome`が`unresolved`から`completed`へ変わる一回だけ、stable resolution IDとresolved timeをRun Recordへ保存する。

public statusは次のように投影する。

| Run state | Public status |
| --- | --- |
| runなし | `idle` |
| `running + unresolved` | `working` |
| `waiting + unresolved` | `blocked` |
| `error + unresolved` | `blocked` |
| `ended + unresolved` | `ended_unconfirmed` |
| `completed` | `done` |

process absenceとterminal静止はRun Evidenceであり、それだけではRun Resolutionを作らない。

現行の`Idle <=> run_seq == completed_seq`と「agent不在ならIdle」というinvariantをv3では廃止する。

current executionの有無はcurrent stable run IDで判定する。

Agent Occupantが交代した場合、古いunresolved runのexecutionを`ended`にするが、Run Recordとwaitを保持する。

同じpaneで新しいAgent Epochが始まった後も、retention中のhistorical runをget、waitできる。

operator recoveryはPane snapshotのcurrent durable run pointerと完全一致するRunだけを対象とする。
occupant replacement後のhistorical runはread-onlyであり、別occupantのPane projectionを誤って更新しない。

terminal fingerprintの静止はevidenceだけを追加し、execution phaseとsemantic outcomeを変更しない。

許可する主要遷移は次のとおりとする。

| From | Event | To |
| --- | --- | --- |
| current execution-active runなし | attributed UserPromptSubmit | new runの`running + unresolved` |
| `running`または`error` | permission / user-input request | `waiting + unresolved` |
| `waiting`または`error` | attributed activity | `running + unresolved` |
| unresolvedの任意phase | exact occupant exit / replacement | `ended + unresolved` |
| unresolvedの任意phase | attributed provider completion | `ended + completed(provider)` |
| unresolvedの任意phase | valid operator completion | `ended + completed(operator)` |
| completed | duplicate same event / resolution | state不変、evidenceまたはartifactだけを単調補強 |
| 任意 | terminal静止 | phaseとoutcome不変、evidence追加 |

表にないphase、outcome、resolutionの組み合わせはschema validationで拒否する。

## Persistent state

v3のcanonical private stateは次の四領域だけで構成する。

1. Pane snapshot：current stable run IDとcurrent UI projection。
2. Run Store：run IDごとのatomic record。
3. Operation Store：operation IDごとのatomic record。
4. Body directories：private prompt stagingとResponse Artifact。

provider event ledger、provider ingress WAL、独立resolution audit、artifact capacity manifest、run/operation tombstone、永続diagnostic storeは作らない。

### Run Store

Run RecordはAgent Binding、stable run ID、run sequence、execution phase、semantic outcome、evidence summary、resolutionとoperator audit fields、artifact reference、linked operation reference、applied provider event referencesを持つ。

resolutionのoperator audit fieldsはresolution ID、actor UIDとPID、reasonとdigest、pre/post revision、observed timeとする。

run attributionへ影響するUserPromptSubmitとStopのreference、ingress request ID、payload digest、disposition、receiptは、一つのrunにつき新しい16件までRun Record内へ保存する。同種のretryはcountとfirst/last observed timeへcompactする。

activity、permission、user-input requestはdedupe slotを消費せず、Run Evidence summaryへ集約する。

daemonはcurrent stable run pointerからcurrent Run Recordを直接lookupし、historical Run Recordからだけbounded dedupe indexを再構築する。

provider eventのretry lookupは、このcurrent recordとhistorical indexの順に行い、live Agent Binding解決、event attribution、run allocationより先に完了する。

Pane snapshotへcurrent runを反映する前にRun Recordをdurable化する。途中でcrashした場合は、起動時にexact Agent Bindingと最大`run_seq`を使ってcurrent projectionを再構築する。

current execution-active runはGCしない。

それ以外のhistorical Run Recordはpaneごとに新しい64件、tmux server全体で2048件、かつ30日以内だけを保持し、いずれかを超えたoldest recordからGCする。

削除後のlookupは`run_not_found`を返す。waitはretentionを延長しない。

一つのRun Recordはencoded 16 KiB、Run Evidenceは新しい16件かつaggregate encoded 8 KiBをhard limitとする。

### Operation Store

Operation Storeはappend logではなく、operation IDごとのatomic recordで構成する。

Operation recordはrequest fingerprint、immutable server precondition、prompt digest、dispatch state、optional run reference、result receiptを持ち、prompt bodyを持たない。

settled recordも同じstate generation内ではGCしない。これによりresponse loss後の同一ID retryが新しいdispatchを開始しないことを保証する。

Operation Storeは65,536 recordsかつencoded 128 MiBをhard limitとする。上限到達後は既存recordを削除せず、新規requestを`operation_store_full`として拒否する。

Storeを空にするには、quiesced状態で明示的なstate resetを行う。

### State generation and reset

active state rootは一つだけとし、`state-meta.json`に`ready`または`resetting`のstatus、random state generation、private state format versionを保存する。

`run_ref`と`operation_ref`へgenerationを埋め込み、reset前のreferenceを新しいstateへ再束縛しない。

daemonはbackup generation、partial generation、generation manifestを管理しない。backupが必要なoperatorはreset前にstate rootを外部へ保存する。

offline resetはdaemon停止中に、旧generationと新しいtarget generationを持つ`resetting` markerを`state-meta.json`へ最初にatomic writeする。その後、全private stateを削除して新しい空stateを作り、新generationの`ready` metadataを最後にatomic writeする。

reset途中で停止したstate rootは起動可能とみなさない。daemonは`state_uninitialized`で拒否する。

同じ`--expected-generation`でresetを再実行した場合、`ready` metadataのgenerationまたは`resetting` markerの旧generationと照合する。後者が一致すれば同じtarget generationへのresetを再開する。

```sh
vt agent storage reset \
  --expected-generation "$GENERATION" \
  --confirm-reset \
  --json
```

private state rootまたはmetadataが欠落、metadataが破損、または未対応formatでgenerationを検証できない場合だけ、同じquiescence検証後に`--recover-uninitialized --confirm-reset`でそのexact rootを明示的に初期化または破棄し、新generationを発行できる。有効なmetadataにはこの経路を使えず、旧formatのdecode、migration、別rootへのfallbackは行わない。

resetはdaemon稼働中、in-flight operation、execution-active run、接続中wait、live supported-provider occupantのいずれかを検出した場合に拒否する。

### Atomic persistence and recovery

各recordは0600のtemporary fileへのwrite、file fsync、rename、directory fsyncによってatomic replaceする。

起動時はstate formatとgenerationを検証し、不一致や壊れたcanonical recordがあればfallbackせず起動を拒否する。

unreferenced prompt stagingとartifact temp/final fileはorphanとして起動時に削除する。

provider hookはsingle sequencer内でdedupe lookup、attribution、Run Record atomic replace、current Pane projection更新の順に処理する。

Run Record更新後にresponseを失ったretryは、record内のprovider event referenceから以前のreceiptを返す。

Run Record更新前にdaemonがcrashし、providerがhookをretryしなかった場合、runはunresolvedのまま残り得る。これはstartupをfail closedにせず、diagnosticとoperator completionで回収する。

帰属不能eventはbounded in-memory diagnosticへ記録し、Run Resolutionを作らない。diagnosticはdaemon restartをまたぐcorrectness recordではない。

Operation Storeだけはtmux input開始前のwrite-ahead recordとして扱う。restart recoveryはOperationとcanonical Runを照合してstateを単調に前進させ、tmux inputを再実行しない。

## Provider observation and attribution

v3 durable mutationはP0で実測済みのCodex adapterだけを有効にする。Claude Codeを含むその他のproviderは`unsupported_provider`とする。pane表示など既存projectionの対応と、durable Run/Operation adapterの有効化を混同しない。

hook CLIはinvocationごとにrandom ingress request IDを発行し、daemon responseを失った同一invocationのretryにだけ再利用する。

providerがstable event ID、turn ID、一意なtranscript cursorを提供する場合、それをprovider、session key、hook kindと組み合わせて`provider_event_ref`とする。

同一referenceと同一payload digestのretryは以前のreceiptを返し、同一referenceと異なるdigestまたはrun bindingは`provider_event_conflict`として状態を変更しない。

provider-native turn keyがある場合はstable run IDとの対応を最優先する。

turn keyがないStopは、同じAgent Bindingに属し、event order上そのrunの開始後で、execution-activeなunresolved runが一つだけの場合に限って帰属させる。

stable referenceがないadapterは、UserPromptSubmitとStopのretryが次のlifecycle eventをまたがず、Run Record retentionを超えて再送されないことをP0で実証できる場合だけ有効にする。

stable referenceもこのretry contractも確認できないadapterは有効にしない。

同一provider sessionで新しいUserPromptSubmitが届いた場合、実証済みevent orderingに基づき旧runのexecutionを`ended`へ進め、新しいstable run IDを作る。

stable referenceがないUserPromptSubmitのimmediate retryは、次のlifecycle event前でAgent Binding、prompt digest、current stable run IDが一致する場合だけduplicateとして扱う。

帰属候補が複数または0の場合はdiagnosticに留め、Run Resolutionを作らない。

APIは`provider_ready`という保証値を公開しない。代わりにcurrent Agent Epochへ束縛されたprovider session ID、各lifecycle hookの最終観測時刻、観測process identityを返す。

## Public command surface

v3はv2のread-only `api`、`pane`、`agent list/get/read`を新schemaで維持し、次のrun/operation commandを追加する。

`agent list/get`はpresent occupantにcurrent runがあれば、その`run_ref`、execution phase、semantic outcome、public statusを返す。これが手入力runの`run_ref`取得経路となる。

`vt api schema --json`はcommand tree、closed enum、error code、size/count/time limit、provider compatibilityとattribution contractをmachine-readableに返す。

`vt agent storage status --json`はstate generation、state format version、各領域のrecord/byte usage、hard limit、oldest retained time、in-flight countを返す。

```sh
operation_id="$(uuidgen)"

vt agent prompt "$AGENT_REF" \
  --operation-id "$operation_id" \
  --stdin \
  --json < prompt.txt
```

`operation_id`と`resolution_id`は16文字以上128文字以下のASCII `[A-Za-z0-9_-]`とする。

response loss後は同じ`operation_id`と同じprompt bytesで同じcommandを実行し、operation lookupとして扱う。

```sh
vt agent run get "$RUN_REF" --json
vt agent run wait "$RUN_REF" --until completed --json
vt agent run response "$RUN_REF" --json
vt agent operation get "$OPERATION_REF" --json
vt agent operation wait "$OPERATION_REF" --until prompt-confirmed --json
vt agent run check "$RUN_REF" --json > check.json
vt agent storage status --json
```

occupantの探索と現在状態の確認には`vt agent list`または`vt agent get TARGET`を使う。

Public API v3はglobal stream、wait-any、relative selectorを提供しない。

## Durable Dispatch Operation

### Operation states

Dispatch Operationは次の状態を持つ。

- `prepared`：request identity、prompt digest、private prompt staging blobをdurable化し、tmux childはまだspawnしていない。
- `dispatch_started`：side effectを開始する決定をdurable化した。
- `prompt_confirmed`：対応するUserPromptSubmit hookとraw prompt digestを確認し、`run_ref`を確定した。
- `delivery_unknown`：side effectの有無または対応runを確定できない。
- `rejected`：side effect開始前にrequestを拒否した。

Dispatch Operation自体の成功は`prompt_confirmed`とする。linked Agent Runの完了は別stateとして返す。

`prompt_confirmed`と`rejected`はfinal stateとする。

`delivery_unknown`はsettled ambiguous stateだが、一意なprovider evidenceが後着した場合だけ`prompt_confirmed`へ単調に補強できる。

callerとdaemonは`delivery_unknown`からdispatchを再実行しない。

`delivery_unknown`は同じ完全なAgent Bindingへのambiguous fenceとして残す。late provider
confirmation、occupant replacement、またはquiesced offline resetまで、別operation IDによる
追加dispatchも拒否する。

provider evidenceからOperationへの新規帰属を作れるのは、Operation作成から10秒のconfirmation deadline内だけとする。
deadline内にOperationへlink済みのRunがある場合、そのRunへ一意に帰属する後続evidenceとrestart reconcileはdeadline後も`delivery_unknown`を`prompt_confirmed`へ単調補強できる。新規帰属を作れないdeadline後のevidenceはRunだけを更新し、ambiguous fenceを解除しない。

### Write ordering

prompt staging blob、`prepared` Operation record、CASとper-pane lock、`dispatch_started` Operation record、tmux child spawnの順序を固定する。

`dispatch_started`をfsyncする前にtmux childをspawnしない。

daemonが`prepared`をrestart後に発見した場合、元のconfirmation deadline内は同じ`operation_id`の再要求だけがprivate prompt staging blobからCASとpreflightを再実行する。callerが再要求しないままdeadlineを超えたrecordはside effectなしの`rejected`へ進める。

daemonが`dispatch_started`をrestart後に発見した場合、同じpromptを再送しない。対応runを確認できれば`prompt_confirmed`、確認できなければ`delivery_unknown`とする。

Run RecordがOperationより先に`run_ref`とprompt digestを保存した場合、restart時にAgent Bindingとdigestを照合してOperationを前進させる。

この契約はstate generation内でvde-tmux自身のtmux input spawnを高々一回にするが、exactly-once deliveryを保証しない。

### Request idempotency

daemonはlive targetを解決する前にoperation IDの既存recordをlookupする。

同じ`operation_id`と同じnormalized caller requestを再送した場合は既存Operationを返し、異なるrequestはside effectなしで`operation_conflict`とする。

normalized caller requestはcallerが渡したtarget `agent_ref` bytes、prompt digest、dispatch optionだけを含む。

Agent Binding、expected run sequence、live process fenceは新規Operation受理時のimmutable server preconditionとして保存し、caller request fingerprintへ含めない。

### Confirmation limits

provider UserPromptSubmitはsource operation IDを持たないため、同じAgent Binding、expected run sequence、time window、prompt digestに一致する人間の同一promptとvde-tmux dispatchを区別できない。

`prompt_confirmed` receiptは`confirmation_basis=guarded_window_digest`と`source_attribution=non_exclusive`を返す。

異なるpromptまたはrun sequenceのinterleaveは`delivery_unknown`とする。

callerはdispatch開始からconfirmationまで対象Paneへ直接入力しない。

### Prompt privacy

`vt agent prompt`のprompt bodyは最大65,536 bytesの0600 private staging blobとしてのみ保存し、argv、Operation record、Pane snapshot、response envelope、error、logへ保存しない。汎用の`vt hook emit --prompt`は公開表示用metadataでありargvを使うため、このprivate body契約の対象外とする。

`prompt_confirmed`、`delivery_unknown`、`rejected`のいずれかをdurable化した後にstaging blobを削除してdirectory fsyncする。

## Diagnostic and recovery

### Read-only check

occupantの探索には`vt agent list`または`vt agent get TARGET`を使う。

current durable runのrecovery診断には`vt agent run check RUN_REF --json`を使う。
historical runは`get`と`wait`で診断できるが、Pane CASの対象にはしない。

run checkはcanonical snapshotに加え、process identity、foreground ownership、tmux Pane identity、terminal viewportをfresh fence付きで観測する。

scan-only occupant、active subagent、permission待機、user input待機、新しいprovider activity、Agent Binding不一致、completed runのいずれかではpreconditionを返さない。

次のいずれかを満たす場合だけ、responseへplain JSONの`recovery_precondition`を含める。

- exact bound processの不在を二回連続で観測し、runが`ended_unconfirmed`であり、新しいoccupantが存在しない。
- exact bound processがabsentで、Paneが同じcurrent durable runを指したままreplacement processを二回の観測で固定できる。
- exact bound processがforeground ownerのまま、ANSIなしvisible viewportのcontent-agnostic fingerprintとpane寸法を2秒以上の間隔で二回観測し、その間にRun、Pane、process、foreground ownershipが変化しない。

viewport fingerprintはprovider固有textを認識せず、semantic completionを推論しない。operatorが画面と外部根拠を確認してcompletionを判断し、machineはfreshnessとCASだけを保証する。

### Recovery precondition

`recovery_precondition`は認証tokenではなく、read-only checkで観測した状態を固定するCAS inputである。

次の値を含む。

- server identityと`run_ref`
- complete Agent Binding
- Run Record revision、Pane state ID/revision、current durable Run pointer、lifecycle
- active subagent count
- `exact_present_stable`、`exact_absent`、`replaced_by`のいずれかのprocess expectation
- exact-present時だけ、fingerprint convention version、pane寸法、viewport digest
- evidence digest
- issued timeと60秒のTTL

preconditionはsecretではなく、daemon restartだけでは無効化しない。revision、Agent Binding、evidence、process expectation、TTLのいずれかが一致しなければ無効とする。

`run check --json`の出力全体をfileへ保存し、resolveの`--precondition-file`へ渡せる。

### Operator completion

```sh
resolution_id="$(uuidgen)"

vt agent run resolve "$RUN_REF" \
  --outcome completed \
  --precondition-file check.json \
  --resolution-id "$resolution_id" \
  --reason "Stop hook was not observed" \
  --json
```

API v3で指定できる`--outcome`は`completed`だけとする。

reasonは1 byte以上1024 bytes以下のUTF-8とし、resolution audit fieldsとして同じRun Recordへ保存する。

`resolution_id`は一つの`run_ref`内でidempotency identityとなる。

resolveは同じrunのmatching resolution IDとreason digestを先にlookupし、response loss後のretryには以前のreceiptを返す。

同じrunで同じ`resolution_id`を異なるoutcomeまたはreason digestへ再利用した場合は`resolution_conflict`として拒否する。

new resolutionだけpreconditionとfresh evidenceをdaemon sequencer内で再検証する。

Run Record revision、Pane fence、Agent Binding、active subagent count、process presence、foreground ownership、viewport fingerprintのいずれかが変わっていれば、side effectなしで`stale_precondition`とする。

resolveはPaneへkeyを送らず、Run Recordをatomic replaceしてからcurrent runのPane projectionを更新する。Pane projectionの永続化に失敗した場合、同じresolution IDのretryが既存Runを返す前にprojectionを冪等修復する。

既に別のresolution IDでcompletedになったrunは`run_already_resolved`として既存resolution summaryを返す。

operator completion後に届いたlate Stopは、一意に同じstable run IDへ帰属できる場合だけ、既存resolutionのevidenceとartifactを補強する。

## Response Artifact

vde-tmuxはstable runへ帰属済みのbounded final response derivativeを所有する。vde-monitorはraw hook payload、tool event、transcript path、dashboardを独立して所有する。

vde-tmuxのartifact metadataはvde-monitorのfile path、event reference、schemaへ依存しない。

Codex P0でStop payloadとprovider transcriptの取得元・digest一致を確定した。今後providerを追加する場合、hook payloadに完全なbodyがなければ、adapterがtranscriptからfinal responseを抽出できるかをP0で検証し、完全性を証明できないadapterは有効化しない。

artifact metadataは次を持つ。

- `run_ref`とoptional `operation_ref`
- provider session IDとobserved process identity
- original/stored byte count
- original response/stored body SHA-256 digest
- provider completenessとして`complete`または`unknown`
- store completenessとして`complete`、`truncated`、`unavailable`、`expired`
- source、encoding、observed time

artifact fileは0600、directoryは0700とする。

一つのbodyは最大512 KiB、server全体は最大64 MiBかつ4096 filesをcandidate limitとし、P0後にfreezeする。

上限を超えるbodyはUTF-8境界を保ったsuffixを保存して`truncated`とする。

保存時はoldest completed runから必要量をGCし、directory scanで実使用量とfile countを確認する。capacity manifestとreservation accountingは持たない。

GCは先にRun Recordのartifact stateを`expired`へatomic replaceし、その後fileをunlinkしてdirectory fsyncする。途中でcrashしたfileはorphanとして起動時に削除する。

new artifactはtemp write、file fsync、rename、directory fsync後にRun Recordへreferenceを保存する。途中でcrashしたfileはorphanになり、存在しないfileをcanonical stateから参照しない。

artifact保存または容量確保に失敗してもprovider completionは`unavailable` metadata付きでRun Recordへ保存する。

duplicate provider completionは同じoriginal digestのartifactだけを単調補強し、異なるdigestで既存bodyを上書きしない。

`vt agent run response RUN_REF`はResponse Artifactだけを返し、terminal captureへfallbackしない。

`semantic_outcome=unresolved`のrunには`run_unresolved`、completedだがbodyを保存できなかったrunには`artifact_unavailable`を返す。

## Wait and read

`vt agent run wait RUN_REF`は最初にretained Run Recordのcurrent stateを評価する。

defaultのmatch対象は`completed`、`waiting`、`error`、`ended_unconfirmed`とする。

`--until completed`はoccupantのabsenceまたはreplacement後もlate provider completionまたはoperator completionまで待機できる。

daemon restart後はretained Run Recordからcompletionとcurrent blocked stateを再評価する。

durable transition journalは追加しない。restart前に発生して既に解除されたtransient blockedをreplayしない。

waitはRun Record retentionを延長せず、待機中にGCされた場合は`run_not_found`を返す。

`vt agent operation wait OPERATION_REF --until prompt-confirmed`は`prompt_confirmed`または`rejected`まで待つ。`delivery_unknown`では再送禁止のtyped ambiguous resultを即時返す。

late confirmationだけを観測する場合は`--follow-unknown --timeout-ms N`を明示する。

`--until run-completed`は`run_ref`確定後にlinked runのcompletionを待ち、operation resultとrun resultを別fieldで返す。

`run_ref`が確定しないままOperationが`delivery_unknown`になった場合は、同じtyped ambiguous resultを即時返す。

`agent prompt`と`agent operation wait`は`delivery_unknown`または`rejected`をreceipt付きの
typed error envelopeとして返す。`agent operation get`だけはterminal stateを成功応答として返す。

## Resource limits

| Resource | Hard limit |
| --- | --- |
| Pane snapshot | encoded 16 MiB total、v3 projection 4 KiB/pane |
| Run Store | historical 64/pane、2048/server、30日、aggregate 96 MiB |
| Run Record / Evidence | 16 KiB/record、16 evidence、8 KiB aggregate evidence |
| Operation Store | 65,536 records、128 MiB aggregate、4 KiB/record |
| prompt body / staging | 65,536 bytes/body、128 records、8 MiB aggregate |
| Response Artifact | candidate 512 KiB/body、4096 files、64 MiB aggregate |
| concurrent snapshot subscription | 48 streams |
| wait timeout | 1 ms以上24時間以下。durable waitは最大1秒backoffのquery connection |
| daemon request frame | encoded 1 MiB |

Run Storeの96 MiBはhistorical record最大32 MiBに、Pane snapshot上限から導かれるcurrent execution-active record最大64 MiBを加えた上限である。Operation Storeのrecord数とbyte数は独立した上限であり、大きいrecordでは128 MiBが先に適用される。

guarded dispatch prompt、unbounded provider ingress、artifact bodyをPane snapshot、global snapshot、list/get summary、operation indexへ埋め込まない。手入力promptとresponseのbounded one-line UI previewはPane snapshotへ保持できる。

private full bodyはrequest処理中だけbounded readし、request完了後にdaemon cacheへ保持しない。

historical Run event dedupe indexは2048 records x 16 eventsから導かれる32,768 entriesを超えない。current Run RecordはPaneのcurrent stable run pointerから直接lookupし、このindexへ重複保持しない。

Operation recordは容量確保のために削除せず、上限時は新規dispatchをfail closedする。Run Recordとartifactはcurrent/active recordを保護した定義済みretention GCだけを許可する。

## Error contract

API v3はv2の`stage`、`side_effect`、`retry_action`を維持する。

新しいclosed error codeは少なくとも次を含む。

- `operation_conflict`
- `operation_not_found`
- `operation_store_full`
- `operation_generation_replaced`
- `run_not_found`
- `run_generation_replaced`
- `run_unresolved`
- `run_already_resolved`
- `target_replaced`
- `unsupported_provider`
- `provider_event_conflict`
- `recovery_not_allowed`
- `stale_precondition`
- `resolution_conflict`
- `storage_capacity_exceeded`
- `state_uninitialized`
- `artifact_unavailable`
- `artifact_expired`

`dispatch_started`以後の不明状態は`delivery_unknown`と`inspect_manually`を返し、自動retryを許可しない。

## Responsibility boundaries

### vde-tmux

vde-tmuxはtmux内のAgent Occupant identity、Agent Run、Dispatch Operation、Run Resolution、Response Artifactを所有する。

vde-tmuxはproviderのraw hook payloadを外部consumerへ中継しない。

### dotfiles

dotfilesはtmux-agent-bridgeのworkflow、provider対応表、hook設定、Codex template反映を所有する。

P0を通過したCodexのbridge transportはv3 cutover後にvt-onlyへ変更する。Claude Codeは
durable adapterがP0を通過するまで既存raw transportをprovider固有の選択として維持し、
durable dispatch開始後のfallback先には使わない。

unsupported provider、inferred occupant、artifact truncation、delivery unknownでは停止して報告し、raw tmuxへfallbackしない。

opencodeはproper provider hook adapterが完成するまでunsupportedとする。

### vde-monitor

vde-monitorはtool粒度のraw hook event、transcript path、summary、dashboardを所有する。

v3 cutoverではvde-monitor-hookを削除しない。hook縮小は別タスクで判断する。

### vde-notifier

vde-notifierは人間向け通知だけを所有し、Run Resolutionのauthorityとして扱わない。

## P0 gates

schema freezeと運用有効化前に次の実測を完了する。model、store、protocolの実装は、一次資料で確認したcandidate fieldを使って先行できるが、実測未完了のadapterを完成扱いにしない。

1. 有効化対象providerで、1行と複数行のpromptのinput bytesとUserPromptSubmit digest境界を実測する。
2. 有効化対象providerで、completion payloadまたはtranscriptからfinal response bodyと完全性を取得できるか実測する。
3. 有効化対象providerのevent/turn/cursor identity、callback retry範囲、実行中のqueued promptを含むlifecycle orderingを実測する。
4. 有効化対象providerのhook fanoutで、一方のfailureが他方の実行を抑止しないことを確認する。

各gateのpass条件を次のように固定する。

- Gate 1：元prompt fileとhook payloadのbyte count、SHA-256、LF数が1行・複数行の両方で一致する。いずれかが一致しなければadapterを有効化しない。
- Gate 2：同じstable turn identityのStop payloadとprovider transcriptの最終assistant textがbyte countとSHA-256で一致する。body欠落時はcompletionを保持してartifactを`unavailable`とし、terminal captureへfallbackしない。
- Gate 3：Codexは`provider + session_id + turn_id + hook_kind`をstable event referenceにでき、queued turnでidentityを再利用せず、通常経路のStopを同じturnへ一意に帰属できる。Claude Codeは同等のauthenticated isolated evidenceが得られるまでdisabledとする。
- Gate 4：同じhook eventにexit 1とexit 0のcollectorを登録し、失敗側の有無にかかわらず成功側markerがUserPromptSubmitとStopの両方で記録される。

probeはraw promptとresponse bodyを保存せず、event、byte count、SHA-256、stable identity hash、monotonic timeだけを0600の結果へ記録する。source schemaだけの確認や非interactive CLIの結果を実機passへ代用しない。

一つでも必要条件が成立しないadapterはv3で有効化しない。Response Artifactのsourceとcandidate limitはgate 2の結果後にfreezeする。

vde-monitor field棚卸しとdotfiles compact-guard修正は並行タスクとし、v3 schema freezeのblockerにしない。

## Candidate version map

| Contract | Before v3 | Implemented v3 |
| --- | ---: | ---: |
| Public Agent API | 2 | 3 |
| Daemon protocol | 14 | 16 |
| PaneState / snapshot schema | 8 | 9 |
| Private state format | none | 1 |

一つでもversionが一致しないclient、daemon、state rootは起動またはrequestを拒否し、cross-version fallbackを行わない。

## Implementation phases

### Phase 1: P0 and state model

P0を実測し、Pane projection、Run Record、provider attribution、Response Artifact sourceをfreezeする。

### Phase 2: Persistent Run and Operation

Run Store、Operation Store、stable references、provider dedupe、bounded artifactを実装する。

### Phase 3: Daemon-owned dispatch

prompt transportをdaemon mutationへ移し、operation idempotency、per-pane lock、restart recoveryを実装する。

### Phase 4: Diagnostic, recovery, and wait

read-only check、operator completion、run/operation wait、response readを実装する。

### Phase 5: Cutover

全Phaseを同一のunreleased v3 contractとして完成させてから、一回のversion cutoverを行う。

## Cutover

runtimeでv2とv3を併存させない。

cutover前に新規Dispatch Operationを停止し、in-flight operation、active execution、接続中waitを0にする。

unresolved runと`delivery_unknown`をoperatorが確認し、旧generationへ束縛されたClaude CodeとCodex sessionを終了する。

旧binaryをdaemon管理外へbackupし、owned hookを外してdaemonをdisabledにしてから、offlineのv2 state rootをbackupする。

既定手順では旧stateをdecodeせず、offline state resetでPane snapshot、Run Store、Operation Store、prompt staging、artifactを空にし、新generationを発行する。

state保持が必要な場合だけ、後方互換対応の影響を整理してone-shot migrationの承認を得る。

事前にstaged installとcandidate hashを確定する。disabled状態でcandidateをinstallし、installed hashを照合してからdaemonをenableし、API、daemon protocol、Pane schema、private state formatの一致を確認する。

Claude CodeとCodex sessionを再起動してSessionStartを再観測させ、scratch serverと常用serverのacceptance後にdotfiles bridgeをv3へ切り替える。

rollbackはquiesced状態で旧binaryとoperator backupを戻す運用手順とし、runtime fallbackとして実装しない。

## Pre-cutover evidence (2026-08-16)

- release candidate: `vt` SHA-256 `5d396e41f8b39c1703784bf8e3e648cbb605621c7714ae4a036843f42e6ae11a`
- release candidate: `vde-tmux` SHA-256 `c02439143e18ce27471b03c3b48a48cc507d5b3ed008b5f29dca9f4e3a7e153c`
- candidate schema: Agent API 3、daemon protocol 15、PaneState 9、private state 1
- provider contract: Codex 0.147.0はenabled、Claude Code 2.1.227はauthenticated P0未完了のためdisabled
- source gates: format、Clippy、通常test 1,036件、ignored tmux test 2件が成功
- isolated gates: 3本のrelease smoke、prompt smoke、operation crash smoke、P0 verify、staged install、external hook integrationが成功
- runtime smokeのcategory warm switchは33回でp95 99.8ms、max 101.7ms。125msの性能gateを維持したままsemantic waitだけを最大5秒へ分離
- independent review R1: `MUST_FIX=0`、`MUST_SIMPLIFY=0`。R1のshould-fix 7件とnit 4件は修正・回帰検証済み。R2はreviewerの利用上限到達により未回収のため、完了済みとは扱わない
- 常用serverは意図的に未変更。確認時点ではv14 daemon、API v2、working Agent 3件（`%3`、`%6`、`%45`）、sidebar 15件のため、quiesce前のinstallを実施していない

## Definition of Done

### 機能完了条件

- [x] Agent Occupant、Agent Run、Dispatch Operationが別identityとしてJSONへ公開され、`run_seq`をidentityに使わない。
- [x] execution phase、semantic outcome、resolutionが分離され、process absenceとterminal静止だけではcompletionにならない。
- [x] occupant replacement後もretained historical runをget、waitでき、recovery mutationはcurrent durable runだけへCASされる。
- [x] provider completionとfresh operator completionだけがRun Resolutionを作る。
- [x] P0を通過したCodex adapterだけがrunを作成・完了し、Claude Codeを含む未検証providerと曖昧なeventはstateを変更しない。
- [x] Run Record内のevent referenceによりresponse loss後のprovider retryが同じreceiptを返す。
- [x] restart後の未期限`prepared` operationは同一requestで再開でき、期限切れはside effectなしでrejectされ、`dispatch_started`後はtmux inputを再送しない。
- [x] 同じ`operation_id`はstate generation内でCLI終了、daemon restart、response loss後も高々一回しかdispatchされない。
- [x] SessionStart未観測のexact Codex processへ最初のpromptをguarded dispatchでき、同一processの最初の`UserPromptSubmit`だけでOperation Bindingのprovider sessionを確定する。
- [x] 未確定provider sessionはRunへ保存されず、process、pane-state ID、agent kind、prompt digest、許可されたAgent Epoch遷移のいずれかが不一致ならOperationへ帰属しない。
- [x] `delivery_unknown`から自動再送せず、confirmationの非排他的性質をreceiptへ返す。
- [x] checkがplain CAS preconditionを返し、resolveがfresh observationを再検証して同じRun Recordへaudit fieldsを保存する。
- [x] matching resolution retryが既存receiptを返し、stale preconditionと二重resolutionを拒否する。
- [x] Response Artifactがrunへ結び付き、完全性、truncation、digest、expiryを返す。
- [x] guarded dispatchのprompt bodyとdurable responseのfull bodyがPane snapshot、Operation record、argv、error、logへ出ない。Pane snapshotにはbounded response previewだけを許可する。
- [x] Pane snapshot、Run Store、Operation Store、body directoriesだけでrestart recoveryできる。
- [x] `api schema`と`agent storage status`がprovider contract、generation、state format、usage、hard limitをmachine-readableに返す。
- [x] vde-monitor raw hookとvde-notifier通知を維持し、unsupported providerへraw tmux fallbackしない。
- [x] Claude CodeとCodexの利用上限を`blocked` / `waiting` / `usage_limit`として公開し、process終了後もopen runとAgent Summaryを保持する。
- [x] sidebarの黄色いcurrent-agent markerがpane、session、category移動へ追従し、focus先がlive agentでない場合は消える。

### テスト完了条件

- [x] Codex実機でP0の4項目を記録してadapterとResponse Artifact sourceをfreezeし、Claude Codeのdurable mutation拒否を検証する。
- [x] operation crash smokeで、`prepared`保存直後のcrashから同一operationを一度だけ再開できることと、tmux dispatch提出直後のcrashが再送なしの`delivery_unknown`になりlate hookで補強されることを検証する。
- [x] Run Record保存後のPane projection補修と、duplicate StopによるOperation/Response Artifact補修をunit testで検証する。
- [x] same operation retry、request conflict、並行prompt、人間のinterleave、digest mismatch、daemon restartを検証する。
- [x] 手入力run、dispatch run、occupant replacement、historical wait、late Stop、ambiguous Stopを検証する。
- [x] provider eventのretry、duplicate、out-of-order、次lifecycle event越しのreplayを実測契約どおり拒否またはdedupeする。
- [x] process exit、terminal静止、active subagent、check後state変更、TTL超過、present/absent/replaced expectationを検証する。
- [x] artifactのUTF-8境界、candidate size、truncation、corruption、permission、directory-scan GC、orphan cleanupを検証する。
- [x] artifact保存失敗時にもprovider completionが`unavailable`として残ることを検証する。
- [x] Run、Operation、prompt staging、artifact、Pane projectionの宣言済みhard limitと満杯時の挙動を検証する。
- [x] state reset中断時にdaemonが起動を拒否し、reset再実行で新generationを作れることを検証する。
- [x] restart後のwaitがdurable current stateを再評価し、過去のtransient blockedをreplayしないことを検証する。
- [x] body retentionがrequest完了後に0へ戻り、dedupe indexが32,768 entriesを超えないことをstress testする。
- [x] `cargo fmt --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked`、`cargo test --locked -- --ignored`が成功する。
- [x] 既存3 smoke scriptと新しいoperation crash smokeが成功する。
- [x] dotfiles bridge、vde-monitor、vde-notifierをscratch環境で回帰確認する。
- [x] 独立レビューでmust-fixとmust-simplifyが0件になる。
- [x] Claude Codeのrate-limit hook、Claude Code/Codexの厳密なlimit文、誤検知除外、scan throttle、process終了後の状態保持をunit testで検証する。
- [x] current-agent markerのexact pane解決、non-agent消灯、複数幅tierの色、selectionとの分離をunit testとUI/UX preflightで検証する。
- [x] SessionStartを省略した隔離Codexで初回prompt、copy mode解除、operation confirmation、Run wait、Response Artifact回収を検証する。

### 運用反映条件

- [x] P0結果、adapter有効化判断、Response Artifact source、candidate limitのfreeze結果を記録する。
- [x] API、daemon protocol、PaneState、private state formatのversionをrelease文書へ記載する。
- [x] Claude Codeの`StopFailure` hook設定、`usage_limit`のJSON表現、原文確認と回復の運用経路をREADMEへ記載する。
- [x] sidebarのactive-session、current-agent、keyboard selectionの視覚的な役割をREADMEへ記載する。
- [ ] in-flight operation、active execution、waitが0であることを記録する。
- [ ] `delivery_unknown`とunresolved runをoperatorが確認し、旧generationのsupported-provider sessionを終了する。
- [ ] 必要なexternal backupとoffline resetの結果を記録する。
- [ ] 旧daemon停止、新binary install、新daemon起動、SessionStart再観測、実server smoke、dotfiles切替の順で反映する。
- [x] installed binary、running daemon、API、Pane schema、private state format、hook configのversion一致を確認する。
- [x] daemon protocol 16のbinaryへ安全に差し替え、dotfiles bridgeのversion gateと一致することを確認する。
- [ ] 常用serverで二重dispatchがなく、restart resume、run recovery、artifact readが成功する。
- [ ] Codex template applyとClaude Code、Codex session restartを完了する。
- [ ] rollback用binary、external state backup、発動条件、復旧確認手順を記録する。
- [ ] 全DoDの証跡が揃うまでtmux-agent-bridgeをv3既定へ切り替えない。
