# vde-monitor 互換性調査

目的: M6 時点で vde-monitor が旧 `@pane_*` option bus または新 `@vde_*` bus を直接読んでいるか確認する。
vde-monitor repo は読み取り専用で検索した。

## 実行コマンド

```bash
rg -n '@pane_|pane_status|pane_agent|pane_prompt|pane_wait|pane_attention|pane_tasks|pane_subagents|@vde_' \
  ~/repos/github.com/yuki-yano/vde-monitor
```

## 結果

2026-07-04 時点の検索結果は 0 件。

```text
@pane_|pane_status|pane_agent|pane_prompt|pane_wait|pane_attention|pane_tasks|pane_subagents|@vde_=0
```

## 判断

vde-monitor 側に旧 `@pane_*` key 直読み依存は見つからなかった。
したがって、vde-tmux の `@vde_*` への契約刷新に伴う vde-monitor 追随変更は M6 時点では不要。

M7 直前にも同じ検索を再実行し、差分がないことを確認する。
