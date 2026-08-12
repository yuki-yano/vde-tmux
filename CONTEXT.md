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
eligibleなtmux clientでexact Paneがactiveになった事実に基づき、そのPaneの観測時点までのUnread Occurrenceを既読にする操作。Window内の他Paneには波及しない。
_Avoid_: Window acknowledgment, focus clear

**Latest Unread Jump**:
global orderが最新のeligibleなUnread Occurrenceへ移動するdaemon action。移動自体はPane Readを行わず、移動後のview観測が既読化する。
_Avoid_: Latest Done jump, sidebar-local selection

**Unread Span**:
Paneが既読状態から最初のUnread Occurrenceを生成してから、Pane Readによって再び既読になるまでの連続した期間。一つのUnread Span内で複数のUnread Occurrenceが発生し得る。
_Avoid_: Notification history, unread session
