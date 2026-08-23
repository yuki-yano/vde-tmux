# vde-tmux

vde-tmuxは、repositoryを単位としてtmux sessionとagentの作業状況を整理し、複数の表示面から同じ作業状態を扱う。

## Language

**Repository**:
category所属と表示順を共有する作業単位。同じGit repositoryのworktreeは一つのRepositoryとして扱い、非Git projectも同等の単位として扱う。
_Avoid_: Session group, project session

**Session**:
Repository上で動作するtmuxの実行単位。一つのRepositoryに複数のSessionが存在できるが、Session自身はcategory所属を持たない。
_Avoid_: Categorized session, session category

**Category**:
Repositoryを整理する名前付きのまとまり。Sessionの有無とは独立して存在できる。
_Avoid_: Session category, group

**Configured Category**:
設定ファイルによって宣言されたCategory。vde-tmux内から所属先として利用できるが、改名と削除は設定ファイルだけが行う。
_Avoid_: Static category

**Dynamic Category**:
vde-tmux内で宣言されたCategory、または設定から消えた後も明示所属を保持するCategory。vde-tmux内から改名と削除を行える。
_Avoid_: Runtime-only category, temporary category

**Uncategorized**:
明示所属も設定による自動分類もないRepositoryが所属するシステム予約Category。明示的な所属先として固定することもできる。
_Avoid_: misc fallback, empty category

**Category Catalog**:
Configured Category、Dynamic Category、Uncategorizedを統合した、そのtmux serverで利用可能なCategoryの集合。
_Avoid_: Category config, category list

**Automatic Classification**:
明示所属を持たないRepositoryのCategoryを、設定されたpath ruleとdefault categoryから決めること。
_Avoid_: Fallback category, inferred session category

**Explicit Membership**:
RepositoryとCategoryの間にユーザー操作で設定され、永続化された所属関係。設定によるAutomatic Classificationより優先する。
_Avoid_: Override category, stored session category

**Effective Membership**:
Explicit Membership、Automatic Classification、Uncategorizedの優先順を適用して得られる、現在有効なRepositoryの所属関係。
_Avoid_: Resolved session category

**Dormant Repository**:
Explicit Membershipや表示順は保持されているが、現在はSessionを一つも持たないRepository。sidebarには表示されない。
_Avoid_: Deleted repository, inactive session

**Unread Occurrence**:
Paneで新しく発生したWaiting、Error、Completedの通知単位。Pane内sequenceとtmux server全体のglobal orderを持つ。
_Avoid_: Unread Done, notification flag

**Pane Read**:
eligibleなtmux clientでexact Paneがactiveになった事実、または利用者の明示操作に基づき、そのPaneの観測時点までのUnread Occurrenceを既読にする操作。
Peek Navigation中の操作元clientによる表示は根拠から除くが、別clientによる表示は引き続き根拠になる。
Window内の他Paneには波及しない。
_Avoid_: Window acknowledgment, focus clear

**Peek Navigation**:
Priority view内の前後Agentへ表示Paneを移しながら、操作元clientによる表示だけをPane Readの根拠から一時的に除くトリアージ操作。
通常のPane移動、click、Enter、Latest Unread Jumpは含まない。
_Avoid_: Read lock, selection-only move, normal jump

**Explicit Pane Read**:
Peek Navigation中の操作元clientが、現在表示しているexact PaneのUnread Occurrenceを意図して既読にする操作。
共有sidebar selectionではなく、そのclientのPeek Navigation対象を操作する。
daemonが受理した発生順までを既読にし、受理後のOccurrenceは同じPaneに留まる間も新しいPeek Lease区間で保護する。
_Avoid_: Mark Done, complete agent, read shared selection

**Latest Unread Jump**:
global orderが最新のeligibleなUnread Occurrenceへ移動するdaemon action。移動自体はPane Readを行わず、移動後のview観測が既読化する。
_Avoid_: Latest Done jump, sidebar-local selection

**Unread Span**:
Paneが既読状態から最初のUnread Occurrenceを生成してから、Pane Readによって再び既読になるまでの連続した期間。一つのUnread Span内で複数のUnread Occurrenceが発生し得る。
_Avoid_: Notification history, unread session

**Pane Pin**:
exact Paneをsidebarの優先位置へ固定する、tmux server内で共有される永続的な表示設定。
Unread、badge、notification、Pane Readとは独立し、対象Paneが消滅すると解除される。
_Avoid_: Priority Unread Pin, read lock, unread marker, saved notification

**Agent Occupant**:
一つのPaneを占有している、種類とOS process identityを特定できるagent process。
_Avoid_: Agent session, pane agent

**Agent Epoch**:
Agent OccupantとProvider Sessionのbindingが変わらない連続した識別期間。SessionStartまたは新しいoccupantの確定によって新しく始まる。
_Avoid_: Process lifetime, tmux session

**Provider Session**:
providerが発行し、hook eventを一つの会話または実行文脈へ束縛するsession identity。
_Avoid_: Agent Occupant, Agent Epoch, tmux session

**Agent Binding**:
Pane Instance、Agent Epoch、agent kind、provider session、OS process identityを組み合わせた、mutation対象の完全な識別条件。
_Avoid_: Agent reference, process ID

**Agent Run**:
一つのAgent Epoch内で、promptの受理から応答の完了または未解決の終了までを表す一回の対話単位。
_Avoid_: Dispatch operation, task, agent session

**Provider Event**:
provider hookから受け取り、provider session、eventまたはturn identity、payload digestとともに帰属を判定する観測入力。
_Avoid_: Run Resolution, raw event bus

**Dispatch Operation**:
vde-tmuxが一つのpromptを一つのAgent Occupantへ渡すために受理した、再開可能な一回の依頼。
_Avoid_: Agent Run, prompt, tmux input

**Run Evidence**:
Agent Runの活動、待機、process終了、provider通知、terminal状態について観測した事実。
_Avoid_: Run outcome, completion

**Execution Phase**:
Agent Runの実行がrunning、waiting、error、endedのどこにあるかを表す、semantic outcomeとは独立した状態。
_Avoid_: Run outcome, lifecycle

**Limited**:
providerのusageまたはsession allowanceを使い切ったためAgent Runが待機していることを表す表示状態。semantic completionでも、即時のユーザー操作を必要とする状態でもなく、再開には実行可能になったことを示す新しいprovider evidenceが必要になる。
_Avoid_: Done, Blocked, rate-limit error

**Run Resolution**:
providerの完了通知またはoperatorの明示操作によって、Agent Runのsemantic outcomeを確定すること。
_Avoid_: Stale inference, process exit, terminal ready

**Response Artifact**:
一つのAgent Runから得た応答本文と完全性を、canonical Pane状態とは分離して保持するbounded record。
_Avoid_: Response preview, terminal capture, Pane snapshot

**Recovery Precondition**:
read-only診断時のRun revision、Agent Binding、activity evidenceを固定する、短命なoperator completion用CAS input。認証tokenではなく、resolve時に全条件を再検証する。
_Avoid_: Completion evidence, recovery result

**Resolution Audit Fields**:
operator completionの依頼者、理由、pre/post revision、冪等性identityを同じRun Recordへ保存する監査field。
_Avoid_: Separate audit store, diagnostic log
