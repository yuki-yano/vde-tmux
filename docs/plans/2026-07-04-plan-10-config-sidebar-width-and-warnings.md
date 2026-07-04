# Plan 10: sidebar width percent と config loader 仕上げ

> 2026-07-04 作成。Plan 09 完了後の追加作業として、sidebar 幅の
> `%` 指定、config pattern の `${VAR}` 展開、config warning の stderr 表面化、
> 未使用 `ghq_root` の整理を実装する。

## 0. 決定事項

| 論点 | 決定 |
|---|---|
| sidebar.width | `64` のような固定桁数と `"10%"` のような割合指定を両方受ける |
| 数値 width | 実 config が `width: 64` で運用中のため後方互換として維持する |
| sidebar.min_width | 既定 `40`。`%` 指定だけに適用し、固定幅には適用しない |
| `%` 解決 | 使用時に対象 window の `#{window_width}` を tmux から読み、floor で桁数にする |
| CLI `--width` | config と同じ `64` / `10%` 構文を受ける |
| `${VAR}` 展開 | `categories.rules[].ghq_patterns[]` と `categories.session_name_rules[].patterns[]` だけを load 時に展開する |
| 未定義 env var | 旧 vtm と同じく config error。新実装では daemon/statusline を止めないため default config + warning として扱う |
| config warning | CLI 境界で stderr に出す。statusline 系の stdout は汚さない |
| `ghq_root` | 旧 vtm では ghq relative path 算出に使った。新実装は project path の suffix match で同等要件を満たすため削除する |

## 1. 設計サマリ

- `src/config/mod.rs` に `SidebarWidth` を追加する。
  `Deserialize` は untagged raw enum で `u16` と `String` を受け、config の string は
  `"10%"` 形式だけを許可する。
  CLI は `FromStr` を使うため、`--width 64` と `--width 10%` の両方を受ける。
- `SidebarConfig` は `width: SidebarWidth` と `min_width: u16` を持つ。
  default は `width = Columns(40)`、`min_width = 40`。
- `src/sidebar/layout.rs` が `SidebarWidth` を受け取り、open/toggle/layout-applied/rail の
  実行時に必要な target window へ `display-message -p -t <target> -F "#{window_width}"`
  を投げて割合を解決する。
  `toggle --all` は window ごとにその時点の幅を解決する。
- config loader は parse 成功後に pattern 展開を行う。
  展開対象は `ghq_patterns` と `session_name_rules.patterns`。
  未定義変数があれば default config + warning にする。
- `run_with_input_at` は `load_config` の warnings を stderr writer に流してから command dispatch する。
  テスト用には writer 注入可能な helper を用意する。
- `ghq_root` は `Config`、schema、README、migration docs、テストから削除する。
  top-level schema の `additionalProperties: false` と挙動を合わせるため、`Config` に
  `deny_unknown_fields` を付ける。

## 2. 共通ルール

- 各タスクは RED(失敗するテスト)→ GREEN(実装)→ 品質ゲート → コミット、の順で進める。
- コミット前に必ず `rtk cargo fmt` を実行し、整形結果を採用する。
- 品質ゲート: `rtk cargo fmt && rtk cargo test && rtk cargo clippy --all-targets -- -D warnings && rtk cargo fmt --check`
- コミットメッセージは日本語。
  複数行は必ずヒアドキュメント形式:

```bash
rtk git commit -m "$(cat <<'EOF'
要約を書く

- 箇条書きを書く
EOF
)"
```

- 検証は scratch tmux(`tmux -L <name> -f /dev/null`)のみ。
- `cargo install` はしない。
- 旧 2 リポジトリは読み取り専用。
- dotfiles は変更しない。

## 3. DoD

### 機能完了条件

- [ ] `sidebar.width: 64` が従来どおり固定 64 桁として動く。
- [ ] `sidebar.width: "10%"` が 640 桁 window で 64 桁に解決される。
- [ ] `%` 指定の結果が `sidebar.min_width` 未満なら min_width にクランプされる。
- [ ] 固定幅指定には min_width が適用されない。
- [ ] `vt sidebar open/toggle/toggle --all/layout-applied/rail --width 10%` が同じ幅解決を使う。
- [ ] `categories.rules[].ghq_patterns[]` と `categories.session_name_rules[].patterns[]` の `${VAR}` が config load 時に展開される。
- [ ] 未定義 env var は warning になり、default config で継続する。
- [ ] 壊れた config の warning が全 command で stderr に出る。
- [ ] statusline 系 command の stdout には warning が混ざらない。
- [ ] `ghq_root` が Config / schema / README / docs/migration.md から削除される。

### テスト完了条件

- [ ] `rtk cargo test sidebar_width` が RED→GREEN 済み。
- [ ] `rtk cargo test config_pattern_env` が RED→GREEN 済み。
- [ ] `rtk cargo test config_warning` が RED→GREEN 済み。
- [ ] `rtk cargo test ghq_root` または schema/doc 検査相当が RED→GREEN 済み。
- [ ] 全品質ゲートが green。
- [ ] `rtk bash scripts/smoke-m6-runtime.sh` が pass。

### 運用反映条件

- [ ] README の config 例に `width: "10%"` と `min_width: 40` が載っている。
- [ ] docs/migration.md の config 例に `width: "10%"` と `min_width: 40` が載っている。
- [ ] 完了報告で、実 config `~/.config/vde/tmux/config.yml` から `ghq_root` を削除する必要があることを明記する。
- [ ] dotfiles は変更しない。

---

## Task 1: sidebar.width 型と schema を追加する

### Step 1: RED — config テストを書く

`src/config/mod.rs` の `#[cfg(test)] mod tests` に追加する。

```rust
#[test]
fn sidebar_width_accepts_columns_and_percent() {
    let columns = serde_yaml_ng::from_str::<Config>("sidebar:\n  width: 64\n").unwrap();
    assert_eq!(columns.sidebar.width, SidebarWidth::Columns(64));
    assert_eq!(columns.sidebar.min_width, 40);

    let percent = serde_yaml_ng::from_str::<Config>("sidebar:\n  width: \"10%\"\n").unwrap();
    assert_eq!(percent.sidebar.width, SidebarWidth::Percent(10));
    assert_eq!(percent.sidebar.min_width, 40);
}

#[test]
fn sidebar_min_width_can_be_overridden() {
    let config = serde_yaml_ng::from_str::<Config>(
        "sidebar:\n  width: \"10%\"\n  min_width: 48\n",
    )
    .unwrap();
    assert_eq!(config.sidebar.width, SidebarWidth::Percent(10));
    assert_eq!(config.sidebar.min_width, 48);
}

#[test]
fn sidebar_width_rejects_invalid_percent() {
    let err = serde_yaml_ng::from_str::<Config>("sidebar:\n  width: \"%\"\n").unwrap_err();
    assert!(err.to_string().contains("expected percentage width"));
}
```

`src/config/schema.rs` のテストに追加する。

```rust
#[test]
fn schema_sidebar_width_accepts_integer_or_percent_string() {
    let schema = config_schema();
    let sidebar = &schema["properties"]["sidebar"]["properties"];
    assert!(sidebar["width"]["oneOf"].is_array());
    assert_eq!(sidebar["min_width"]["type"], "integer");
}
```

### Step 2: RED を確認する

```bash
rtk cargo test sidebar_width
```

Expected: `SidebarWidth` 未定義、または `width` が `u16` のため `"10%"` を parse できず失敗。

### Step 3: GREEN — config 型を実装する

`src/config/mod.rs` に追加する。

```rust
use std::str::FromStr;

use serde::{Deserialize, Deserializer, de};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarWidth {
    Columns(u16),
    Percent(u16),
}

impl Default for SidebarWidth {
    fn default() -> Self {
        Self::Columns(40)
    }
}

impl FromStr for SidebarWidth {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if let Some(percent) = value.strip_suffix('%') {
            let percent = percent
                .parse::<u16>()
                .map_err(|_| format!("expected percentage width like \"10%\", got {value:?}"))?;
            if !(1..=100).contains(&percent) {
                return Err(format!("expected percentage width from 1% to 100%, got {value:?}"));
            }
            return Ok(Self::Percent(percent));
        }
        let columns = value
            .parse::<u16>()
            .map_err(|_| format!("expected sidebar width like 64 or \"10%\", got {value:?}"))?;
        if columns == 0 {
            return Err("sidebar width must be greater than 0".to_string());
        }
        Ok(Self::Columns(columns))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawSidebarWidth {
    Columns(u16),
    Percent(String),
}

impl<'de> Deserialize<'de> for SidebarWidth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match RawSidebarWidth::deserialize(deserializer)? {
            RawSidebarWidth::Columns(columns) if columns > 0 => Ok(Self::Columns(columns)),
            RawSidebarWidth::Columns(_) => {
                Err(de::Error::custom("sidebar width must be greater than 0"))
            }
            RawSidebarWidth::Percent(value) if value.trim().ends_with('%') => {
                value.parse().map_err(de::Error::custom)
            }
            RawSidebarWidth::Percent(value) => Err(de::Error::custom(format!(
                "expected percentage width like \"10%\", got {value:?}"
            ))),
        }
    }
}
```

`SidebarConfig` を置き換える。

```rust
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub width: SidebarWidth,
    pub min_width: u16,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            width: SidebarWidth::default(),
            min_width: 40,
        }
    }
}
```

`empty_yaml_yields_full_defaults` の assertion を更新する。

```rust
assert_eq!(config.sidebar.width, SidebarWidth::Columns(40));
assert_eq!(config.sidebar.min_width, 40);
```

`src/config/schema.rs` の `sidebar.width` を置き換える。

```rust
"width": {
    "oneOf": [
        { "type": "integer", "minimum": 1 },
        { "type": "string", "pattern": "^(100|[1-9][0-9]?)%$" }
    ]
},
"min_width": { "type": "integer", "minimum": 1 }
```

### Step 4: GREEN を確認する

```bash
rtk cargo test sidebar_width
rtk cargo test schema_sidebar_width
```

### Step 5: 品質ゲートとコミット

```bash
rtk cargo fmt && rtk cargo test && rtk cargo clippy --all-targets -- -D warnings && rtk cargo fmt --check
rtk git add src/config/mod.rs src/config/schema.rs
rtk git commit -m "$(cat <<'EOF'
sidebar 幅の percent config を追加する

- sidebar.width で固定桁数と percent 文字列を受ける
- sidebar.min_width を追加する
- config schema に width/min_width を反映する
EOF
)"
```

---

## Task 2: sidebar layout と CLI width を percent 対応にする

### Step 1: RED — layout と CLI テストを書く

`src/sidebar/layout.rs` の tests に追加する。

```rust
#[test]
fn open_resolves_percent_width_from_window_width() {
    let mock = MockTmuxRunner::new();
    mock.stub(
        &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
        "%1\t\t640\n",
    );
    mock.stub(
        &["display-message", "-p", "-t", "@1", "-F", "#{window_width}"],
        "640\n",
    );
    mock.stub(
        &["display-message", "-p", "-t", "@1", "-F", "#{window_layout}"],
        "layout-before\n",
    );
    mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
    mock.stub(&["set-option", "-w", "-t", "@1", KEY_LAYOUT_BASELINE, "layout-before"], "");
    mock.stub(&["set-option", "-w", "-t", "@1", KEY_LAYOUT_PANES, "%1"], "");
    mock.stub(
        &["split-window", "-t", "@1", "-hbf", "-l", "64", "'/tmp/vt' sidebar attach"],
        "",
    );

    open(&mock, "@1", &exe(), SidebarWidth::Percent(10), 40).unwrap();
}

#[test]
fn percent_width_is_clamped_to_min_width() {
    let mock = MockTmuxRunner::new();
    mock.stub(
        &["display-message", "-p", "-t", "@1", "-F", "#{window_width}"],
        "320\n",
    );
    assert_eq!(
        resolve_width(&mock, "@1", SidebarWidth::Percent(10), 40).unwrap(),
        40
    );
}

#[test]
fn fixed_width_is_not_clamped_to_min_width() {
    let mock = MockTmuxRunner::new();
    assert_eq!(
        resolve_width(&mock, "@1", SidebarWidth::Columns(20), 40).unwrap(),
        20
    );
}

#[test]
fn rail_resolves_percent_width_when_restoring_normal_width() {
    let mock = MockTmuxRunner::new();
    mock.stub(
        &["list-panes", "-t", "@1", "-F", SIDEBAR_PANE_FORMAT],
        "%9\t1\t2\n",
    );
    mock.stub(
        &["display-message", "-p", "-t", "@1", "-F", "#{window_width}"],
        "640\n",
    );
    mock.stub(&["resize-pane", "-t", "%9", "-x", "64"], "");

    rail(&mock, "@1", SidebarWidth::Percent(10), 40).unwrap();
}
```

`src/cli/tests/sidebar.rs` に追加する。

```rust
#[test]
fn dispatch_sidebar_open_accepts_percent_width() {
    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "{} sidebar attach",
        shell_quote_for_test(&exe.display().to_string())
    );
    mock.stub(
        &["list-panes", "-t", "@1", "-F", crate::sidebar::layout::SIDEBAR_PANE_FORMAT],
        "%1\t\t640\n",
    );
    mock.stub(&["display-message", "-p", "-t", "@1", "-F", "#{window_width}"], "640\n");
    mock.stub(&["display-message", "-p", "-t", "@1", "-F", "#{window_layout}"], "layout-before\n");
    mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
    mock.stub(&["set-option", "-w", "-t", "@1", crate::options::KEY_LAYOUT_BASELINE, "layout-before"], "");
    mock.stub(&["set-option", "-w", "-t", "@1", crate::options::KEY_LAYOUT_PANES, "%1"], "");
    mock.stub(&["split-window", "-t", "@1", "-hbf", "-l", "64", &command], "");

    crate::cli::run_with(
        ["vt", "sidebar", "open", "--window", "@1", "--width", "10%"],
        &mock,
        &env(),
    )
    .unwrap();
}
```

### Step 2: RED を確認する

```bash
rtk cargo test open_resolves_percent_width_from_window_width
rtk cargo test dispatch_sidebar_open_accepts_percent_width
```

Expected: 関数 signature と CLI `u16` parser が未対応で失敗。

### Step 3: GREEN — layout と CLI を実装する

`src/sidebar/layout.rs` の import と signature を更新する。

```rust
use crate::config::SidebarWidth;

pub fn open(
    runner: &dyn TmuxRunner,
    target: &str,
    self_exe: &Path,
    width: SidebarWidth,
    min_width: u16,
) -> Result<()> {
    if find_sidebar_pane(runner, target)?.is_some() {
        return Ok(());
    }
    open_unchecked(runner, target, self_exe, width, min_width)
}
```

`toggle` / `toggle_all` / `rail` / `layout_applied` / `open_unchecked` も同じ
`SidebarWidth, min_width` を受けるように更新する。

```rust
fn resolve_width(
    runner: &dyn TmuxRunner,
    target: &str,
    width: SidebarWidth,
    min_width: u16,
) -> Result<u16> {
    match width {
        SidebarWidth::Columns(columns) => Ok(columns),
        SidebarWidth::Percent(percent) => {
            let output = runner.run(&[
                "display-message",
                "-p",
                "-t",
                target,
                "-F",
                "#{window_width}",
            ])?;
            let window_width = output
                .trim()
                .parse::<u32>()
                .with_context(|| format!("failed to parse window width for {target}"))?;
            let resolved = window_width.saturating_mul(percent as u32) / 100;
            Ok((resolved as u16).max(min_width))
        }
    }
}
```

`open_unchecked` の split 幅は `resolve_width` の結果を使う。

```rust
let width = resolve_width(runner, target, width, min_width)?;
runner.run(&[
    "split-window",
    "-t",
    target,
    "-hbf",
    "-l",
    &width.to_string(),
    &command,
])?;
```

`rail` は rail から通常幅へ戻すときだけ解決する。

```rust
let next_width = if sidebar.width <= RAIL_WIDTH {
    resolve_width(runner, target, normal_width, min_width)?
} else {
    RAIL_WIDTH
};
```

`src/cli/sidebar.rs` は width の型と parser を更新する。

```rust
use crate::config::SidebarWidth;

fn parse_sidebar_width(value: &str) -> Result<SidebarWidth, String> {
    value.parse()
}
```

各 `width` arg を次の形にする。

```rust
#[arg(long, value_parser = parse_sidebar_width)]
width: Option<SidebarWidth>,
```

dispatch では config default を使う。

```rust
let width = width.unwrap_or(config.sidebar.width);
let min_width = config.sidebar.min_width;
crate::sidebar::layout::open(runner, &target, &std::env::current_exe()?, width, min_width)?;
```

### Step 4: GREEN を確認する

```bash
rtk cargo test sidebar_width
rtk cargo test dispatch_sidebar_open_accepts_percent_width
rtk cargo test rail_resolves_percent_width_when_restoring_normal_width
```

### Step 5: 品質ゲートとコミット

```bash
rtk cargo fmt && rtk cargo test && rtk cargo clippy --all-targets -- -D warnings && rtk cargo fmt --check
rtk git add src/sidebar/layout.rs src/cli/sidebar.rs src/cli/tests/sidebar.rs
rtk git commit -m "$(cat <<'EOF'
sidebar layout で percent 幅を解決する

- open/toggle/toggle --all/layout-applied/rail の幅解決を layout に集約
- CLI --width で 10% 形式を受ける
- percent 幅は window_width から floor 計算し min_width でクランプする
EOF
)"
```

---

## Task 3: config pattern の `${VAR}` 展開と `ghq_root` 削除

### Step 1: RED — loader と schema テストを書く

`src/config/load.rs` の tests に追加する。

```rust
#[test]
fn config_pattern_env_expands_ghq_and_session_patterns() {
    let loaded = parse_config_with_env(
        r#"
categories:
  rules:
    - category: work
      ghq_patterns:
        - github.com/${WORK_GHQ_OWNER}/*
  session_name_rules:
    - category: work
      patterns:
        - ${WORK_PREFIX}-*
"#,
        &env(&[
            ("WORK_GHQ_OWNER", "acme"),
            ("WORK_PREFIX", "corp"),
        ]),
    );
    assert!(loaded.warnings.is_empty());
    assert_eq!(
        loaded.config.categories.rules[0].ghq_patterns[0],
        "github.com/acme/*"
    );
    assert_eq!(
        loaded.config.categories.session_name_rules[0].patterns[0],
        "corp-*"
    );
}

#[test]
fn config_pattern_env_missing_var_returns_default_with_warning() {
    let loaded = parse_config_with_env(
        "categories:\n  rules:\n    - category: work\n      ghq_patterns:\n        - github.com/${WORK_GHQ_OWNER}/*\n",
        &env(&[]),
    );
    assert_eq!(loaded.config, Config::default());
    assert_eq!(loaded.warnings.len(), 1);
    assert!(loaded.warnings[0].contains("WORK_GHQ_OWNER"));
}
```

`src/config/mod.rs` に `ghq_root` 削除確認テストを追加する。

```rust
#[test]
fn ghq_root_is_no_longer_accepted_as_top_level_config() {
    let err = serde_yaml_ng::from_str::<Config>("ghq_root: ~/repos\n").unwrap_err();
    assert!(err.to_string().contains("unknown field"));
}
```

`src/config/schema.rs` のテストを更新する。

```rust
for key in ["categories", "statusline", "sidebar", "daemon"] {
    assert!(properties.contains_key(key), "missing schema property {key}");
}
assert!(!properties.contains_key("ghq_root"));
```

### Step 2: RED を確認する

```bash
rtk cargo test config_pattern_env
rtk cargo test ghq_root
```

Expected: `parse_config_with_env` 未定義、または `ghq_root` がまだ受理されて失敗。

### Step 3: GREEN — 展開と削除を実装する

`src/config/mod.rs` の `Config` から `ghq_root` を削除し、top-level unknown を拒否する。

```rust
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub categories: CategoriesConfig,
    pub statusline: StatuslineConfig,
    pub sidebar: SidebarConfig,
    pub daemon: DaemonConfig,
}
```

`src/config/load.rs` に env 付き parser と展開関数を追加する。

```rust
pub fn parse_config(yaml: &str) -> LoadedConfig {
    parse_config_with_env(yaml, &BTreeMap::new())
}

pub fn parse_config_with_env(yaml: &str, env: &BTreeMap<String, String>) -> LoadedConfig {
    if yaml.trim().is_empty() {
        return LoadedConfig {
            config: Config::default(),
            warnings: Vec::new(),
        };
    }
    let deserializer = serde_yaml_ng::Deserializer::from_str(yaml);
    match serde_path_to_error::deserialize::<_, Config>(deserializer) {
        Ok(mut config) => match expand_config_patterns(&mut config, env) {
            Ok(()) => LoadedConfig {
                config,
                warnings: Vec::new(),
            },
            Err(warning) => LoadedConfig {
                config: Config::default(),
                warnings: vec![warning],
            },
        },
        Err(error) => LoadedConfig {
            config: Config::default(),
            warnings: vec![format!("invalid config (path: {}): {error}", error.path())],
        },
    }
}

fn expand_config_patterns(config: &mut Config, env: &BTreeMap<String, String>) -> Result<(), String> {
    for (rule_index, rule) in config.categories.rules.iter_mut().enumerate() {
        for (pattern_index, pattern) in rule.ghq_patterns.iter_mut().enumerate() {
            *pattern = expand_pattern(
                pattern,
                env,
                &format!("categories.rules.{rule_index}.ghq_patterns.{pattern_index}"),
            )?;
        }
    }
    for (rule_index, rule) in config.categories.session_name_rules.iter_mut().enumerate() {
        for (pattern_index, pattern) in rule.patterns.iter_mut().enumerate() {
            *pattern = expand_pattern(
                pattern,
                env,
                &format!("categories.session_name_rules.{rule_index}.patterns.{pattern_index}"),
            )?;
        }
    }
    Ok(())
}
```

`expand_pattern` は `${VAR}` の valid name だけを置換し、未定義なら warning を返す。

```rust
fn expand_pattern(
    value: &str,
    env: &BTreeMap<String, String>,
    path: &str,
) -> Result<String, String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            output.push_str(&rest[start..]);
            return Ok(output);
        };
        let name = &after_start[..end];
        if !is_env_name(name) {
            output.push_str(&rest[start..start + 3 + end]);
            rest = &after_start[end + 1..];
            continue;
        }
        let Some(replacement) = env.get(name) else {
            return Err(format!(
                "invalid config (path: {path}): environment variable {name} is not defined"
            ));
        };
        output.push_str(replacement);
        rest = &after_start[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn is_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}
```

`load_config` は `parse_config_with_env(&content, env)` を呼ぶ。

`src/config/schema.rs` から top-level `ghq_root` を削除する。
README と docs/migration.md の config 例からも `ghq_root` を削除する。

### Step 4: GREEN を確認する

```bash
rtk cargo test config_pattern_env
rtk cargo test ghq_root
rtk cargo test schema_contains_top_level_sections
```

### Step 5: 品質ゲートとコミット

```bash
rtk cargo fmt && rtk cargo test && rtk cargo clippy --all-targets -- -D warnings && rtk cargo fmt --check
rtk git add src/config/mod.rs src/config/load.rs src/config/schema.rs README.md docs/migration.md
rtk git commit -m "$(cat <<'EOF'
config pattern の環境変数展開を追加する

- ghq_patterns と session_name_rules.patterns の ${VAR} を load 時に展開
- 未定義 env var は default config + warning にする
- 未使用 ghq_root を config と docs から削除する
EOF
)"
```

---

## Task 4: config warning を CLI stderr に出す

### Step 1: RED — stderr writer テストを書く

`src/cli/tests.rs` に追加する。

```rust
#[test]
fn config_warning_is_written_to_stderr_without_polluting_statusline_stdout() {
    let config_home = std::env::temp_dir().join(format!(
        "vde-tmux-broken-config-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let config_dir = config_home.join("vde").join("tmux");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.yml"), "daemon:\n  poll_ms: [broken\n").unwrap();

    let env = BTreeMap::from([(
        "XDG_CONFIG_HOME".to_string(),
        config_home.display().to_string(),
    )]);
    let mock = MockTmuxRunner::new();
    let format = crate::session::session_list_format();
    mock.stub(
        &["list-sessions", "-F", &format],
        "main\u{1f}1\u{1f}100\u{1f}misc\u{1f}\u{1f}\u{1f}\n",
    );
    mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");

    let mut stderr = Vec::new();
    let output = run_with_input_at_writing_warnings(
        ["vt", "statusline-category"],
        "",
        &mock,
        &env,
        0,
        &mut stderr,
    )
    .unwrap()
    .unwrap();

    let stderr = String::from_utf8(stderr).unwrap();
    assert!(stderr.contains("vde-tmux config warning: invalid config"));
    assert!(!output.contains("invalid config"));
    std::fs::remove_dir_all(config_home).unwrap();
}
```

### Step 2: RED を確認する

```bash
rtk cargo test config_warning_is_written_to_stderr_without_polluting_statusline_stdout
```

Expected: `run_with_input_at_writing_warnings` 未定義、または stderr が空で失敗。

### Step 3: GREEN — CLI warning writer を実装する

`src/cli/mod.rs` に `Write` を import する。

```rust
use std::io::{Read, Write};
```

`run_with_input_at` を writer 注入 helper に委譲する。

```rust
pub fn run_with_input_at<I, T>(
    args: I,
    input: &str,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    now_epoch: i64,
) -> Result<Option<String>>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut stderr = std::io::stderr();
    run_with_input_at_writing_warnings(args, input, runner, env, now_epoch, &mut stderr)
}

pub(crate) fn run_with_input_at_writing_warnings<I, T, W>(
    args: I,
    input: &str,
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    now_epoch: i64,
    warning_writer: &mut W,
) -> Result<Option<String>>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
    W: Write,
{
    let cli = Cli::try_parse_from(args)?;
    let loaded = load_config(env);
    emit_config_warnings(&loaded.warnings, warning_writer)?;
    let config = loaded.config;
    // 以降は既存 match 本体を移動する。
}

fn emit_config_warnings<W: Write>(warnings: &[String], writer: &mut W) -> Result<()> {
    for warning in warnings {
        writeln!(writer, "vde-tmux config warning: {warning}")?;
    }
    Ok(())
}
```

既存 `run_with_input_at` の command dispatch match は
`run_with_input_at_writing_warnings` の中へそのまま移す。

### Step 4: GREEN を確認する

```bash
rtk cargo test config_warning_is_written_to_stderr_without_polluting_statusline_stdout
```

### Step 5: 品質ゲートとコミット

```bash
rtk cargo fmt && rtk cargo test && rtk cargo clippy --all-targets -- -D warnings && rtk cargo fmt --check
rtk git add src/cli/mod.rs src/cli/tests.rs
rtk git commit -m "$(cat <<'EOF'
config warning を CLI stderr に出力する

- load_config の warnings を全 command の CLI 境界で表示
- statusline stdout と warning stderr を分離してテストする
EOF
)"
```

---

## Task 5: docs と smoke を更新する

### Step 1: RED — docs/schema 検査と smoke 期待を確認する

次の確認を実行する。

```bash
rtk cargo run --bin vt -- config schema | rtk rg 'min_width|oneOf'
rtk rg -n 'ghq_root' README.md docs/migration.md src/config/schema.rs src/config/mod.rs
```

Expected: schema は `min_width|oneOf` を含み、`ghq_root` は対象から消えている。

### Step 2: GREEN — README と migration を確定する

`README.md` の config 例を次の形にする。

```yaml
categories:
  default_category: misc
statusline:
  agent_badge:
    enabled: true
  session_badge:
    enabled: true
    suffix: " "
sidebar:
  width: "10%"
  min_width: 40
daemon:
  poll_ms: 1000
```

`docs/migration.md` の config 例も同じく `ghq_root` を削除し、sidebar を更新する。

```yaml
sidebar:
  width: "10%"
  min_width: 40
```

`docs/migration.md` の Config 節に一文追加する。

```markdown
`ghq_root` は新実装では使わないため削除する。
`~/.config/vde/tmux/config.yml` に残っている場合は、M7 切替前に消す。
```

### Step 3: smoke と品質ゲートを実行する

```bash
rtk cargo fmt && rtk cargo test && rtk cargo clippy --all-targets -- -D warnings && rtk cargo fmt --check
rtk bash scripts/smoke-m6-runtime.sh
```

Expected:

```text
subscribe snapshot ok
capture detect ok
session badge blocked ok
session badge done ok
statusline badge render ok
input redraw state ok
query response ok
session badge cleanup ok
M6 runtime smoke ok
```

### Step 4: コミット

```bash
rtk git add README.md docs/migration.md docs/plans/2026-07-04-plan-10-config-sidebar-width-and-warnings.md
rtk git commit -m "$(cat <<'EOF'
Plan 10 の docs と検証記録を更新する

- sidebar width percent の config 例を README と migration に追加
- ghq_root 削除時の実 config 注意点を migration に記載
- Plan 10 の実装計画書を追加
EOF
)"
```
