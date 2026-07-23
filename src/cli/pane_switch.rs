use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::pane_state::store::tmux_command_string;
use crate::tmux::TmuxRunner;

const FIELD_SEPARATOR: char = '\u{1f}';
const SELECTION_CHANGED_SENTINEL: &str = "__vde_pane_switch_selection_changed__";
const NVIM_CURSOR_Y_OPTION: &str = "@vde_nvim_cursor_y";
const NVIM_CURSOR_X_OPTION: &str = "@vde_nvim_cursor_x";
const NVIM_DIRECTION_OPTION: &str = "@vde_nvim_select_direction";
const NVIM_CYCLE_OPTION: &str = "@vde_nvim_is_cycle";
const NVIM_TARGET_PID_OPTION: &str = "@vde_nvim_target_pane_pid";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum PaneSwitchDirection {
    Left,
    Right,
    Up,
    Down,
}

impl PaneSwitchDirection {
    fn neovim_value(self) -> &'static str {
        match self {
            Self::Left => "L",
            Self::Right => "R",
            Self::Up => "U",
            Self::Down => "D",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pane {
    pane_id: String,
    pane_pid: u32,
    active: bool,
    cursor_x: u32,
    cursor_y: u32,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    current_command: String,
    sidebar: bool,
    floating: bool,
}

pub(crate) fn switch(
    runner: &dyn TmuxRunner,
    direction: PaneSwitchDirection,
    source_pane_id: &str,
    source_pane_pid: u32,
) -> Result<()> {
    validate_pane_id(source_pane_id)?;
    if source_pane_pid == 0 {
        bail!("source pane PID must be positive");
    }

    let panes = query_panes(runner, source_pane_id)?;
    let source = panes
        .iter()
        .find(|pane| pane.pane_id == source_pane_id && pane.pane_pid == source_pane_pid)
        .context("source pane instance no longer exists")?;
    if !source.active {
        bail!("source pane is no longer active");
    }
    let Some((target, is_cycle)) = choose_target(&panes, source, direction) else {
        return Ok(());
    };

    let action = switch_action(target, source, direction, is_cycle);
    let target_guard = format!(
        "#{{&&:#{{==:#{{pane_pid}},{}}},#{{&&:#{{!=:#{{@vde_sidebar}},1}},#{{!=:#{{pane_floating_flag}},1}}}}}}",
        target.pane_pid
    );
    let guarded_target = tmux_command_string(&[
        "if-shell".to_string(),
        "-F".to_string(),
        "-t".to_string(),
        target.pane_id.clone(),
        target_guard,
        action,
        format!("display-message -p '{SELECTION_CHANGED_SENTINEL}'"),
    ]);
    let source_guard =
        format!("#{{&&:#{{==:#{{pane_pid}},{source_pane_pid}}},#{{==:#{{pane_active}},1}}}}");
    let source_mismatch_command = format!("display-message -p '{SELECTION_CHANGED_SENTINEL}'");
    let args = [
        "if-shell",
        "-F",
        "-t",
        source_pane_id,
        &source_guard,
        &guarded_target,
        &source_mismatch_command,
    ];
    let output = runner.run(&args)?;
    if output
        .lines()
        .any(|line| line.trim() == SELECTION_CHANGED_SENTINEL)
    {
        bail!("pane selection changed before the switch could be applied");
    }
    Ok(())
}

fn query_panes(runner: &dyn TmuxRunner, source_pane_id: &str) -> Result<Vec<Pane>> {
    let format = [
        "#{pane_id}",
        "#{pane_pid}",
        "#{pane_active}",
        "#{cursor_x}",
        "#{cursor_y}",
        "#{pane_left}",
        "#{pane_top}",
        "#{pane_width}",
        "#{pane_height}",
        "#{pane_current_command}",
        "#{@vde_sidebar}",
        "#{pane_floating_flag}",
    ]
    .join(&FIELD_SEPARATOR.to_string());
    runner
        .run(&["list-panes", "-t", source_pane_id, "-F", &format])?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_pane)
        .collect()
}

fn parse_pane(row: &str) -> Result<Pane> {
    let fields = row.split(FIELD_SEPARATOR).collect::<Vec<_>>();
    if fields.len() != 12 {
        bail!("pane switch row has {} fields, expected 12", fields.len());
    }
    validate_pane_id(fields[0])?;
    let number = |index: usize, name: &str| -> Result<u32> {
        fields[index]
            .parse::<u32>()
            .with_context(|| format!("pane switch row has invalid {name} {:?}", fields[index]))
    };
    let boolean = |index: usize, name: &str| -> Result<bool> {
        match fields[index] {
            "" | "0" => Ok(false),
            "1" => Ok(true),
            value => bail!("pane switch row has invalid {name} {value:?}"),
        }
    };
    let width = number(7, "pane width")?;
    let height = number(8, "pane height")?;
    if width == 0 || height == 0 {
        bail!("pane switch row has an empty pane geometry");
    }
    Ok(Pane {
        pane_id: fields[0].to_string(),
        pane_pid: number(1, "pane PID")?,
        active: boolean(2, "active flag")?,
        cursor_x: number(3, "cursor x")?,
        cursor_y: number(4, "cursor y")?,
        left: number(5, "pane left")?,
        top: number(6, "pane top")?,
        width,
        height,
        current_command: fields[9].to_string(),
        sidebar: boolean(10, "sidebar marker")?,
        floating: boolean(11, "floating flag")?,
    })
}

fn choose_target<'a>(
    panes: &'a [Pane],
    source: &Pane,
    direction: PaneSwitchDirection,
) -> Option<(&'a Pane, bool)> {
    let absolute_x = source
        .left
        .saturating_add(source.cursor_x.min(source.width - 1));
    let absolute_y = source
        .top
        .saturating_add(source.cursor_y.min(source.height - 1));
    let mut best: Option<(&Pane, u32)> = None;
    let mut edge: Option<(&Pane, u32)> = None;

    for pane in panes {
        if pane.pane_id == source.pane_id || pane.sidebar || pane.floating {
            continue;
        }
        let aligned = match direction {
            PaneSwitchDirection::Left | PaneSwitchDirection::Right => {
                (pane.top..pane.top.saturating_add(pane.height)).contains(&absolute_y)
            }
            PaneSwitchDirection::Up | PaneSwitchDirection::Down => {
                (pane.left..pane.left.saturating_add(pane.width)).contains(&absolute_x)
            }
        };
        if !aligned {
            continue;
        }

        let edge_position = match direction {
            PaneSwitchDirection::Left => pane.left.saturating_add(pane.width),
            PaneSwitchDirection::Right => pane.left,
            PaneSwitchDirection::Up => pane.top.saturating_add(pane.height),
            PaneSwitchDirection::Down => pane.top,
        };
        let replace_edge = edge.is_none_or(|(_, position)| match direction {
            PaneSwitchDirection::Left | PaneSwitchDirection::Up => edge_position > position,
            PaneSwitchDirection::Right | PaneSwitchDirection::Down => edge_position < position,
        });
        if replace_edge {
            edge = Some((pane, edge_position));
        }

        let distance = match direction {
            PaneSwitchDirection::Left if pane.left < source.left => source
                .left
                .checked_sub(pane.left.saturating_add(pane.width)),
            PaneSwitchDirection::Right if pane.left > source.left => pane
                .left
                .checked_sub(source.left.saturating_add(source.width)),
            PaneSwitchDirection::Up if pane.top < source.top => {
                source.top.checked_sub(pane.top.saturating_add(pane.height))
            }
            PaneSwitchDirection::Down if pane.top > source.top => pane
                .top
                .checked_sub(source.top.saturating_add(source.height)),
            _ => None,
        };
        if let Some(distance) = distance
            && best.is_none_or(|(_, best_distance)| distance < best_distance)
        {
            best = Some((pane, distance));
        }
    }

    best.map(|(pane, _)| (pane, false))
        .or_else(|| edge.map(|(pane, _)| (pane, true)))
}

fn switch_action(
    target: &Pane,
    source: &Pane,
    direction: PaneSwitchDirection,
    is_cycle: bool,
) -> String {
    let mut commands = Vec::new();
    if is_neovim(&target.current_command) {
        let absolute_x = source
            .left
            .saturating_add(source.cursor_x.min(source.width - 1));
        let absolute_y = source
            .top
            .saturating_add(source.cursor_y.min(source.height - 1));
        let relative_x = absolute_x.saturating_sub(target.left).min(target.width - 1);
        let relative_y = absolute_y.saturating_sub(target.top).min(target.height - 1);
        let percent_x = relative_x * 100 / target.width;
        let percent_y = relative_y * 100 / target.height;
        for (option, value) in [
            (NVIM_CURSOR_Y_OPTION, percent_y.to_string()),
            (NVIM_CURSOR_X_OPTION, percent_x.to_string()),
            (NVIM_DIRECTION_OPTION, direction.neovim_value().to_string()),
            (NVIM_CYCLE_OPTION, is_cycle.to_string()),
            (NVIM_TARGET_PID_OPTION, target.pane_pid.to_string()),
        ] {
            if !commands.is_empty() {
                commands.push(";".to_string());
            }
            commands.extend([
                "set-option".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                target.pane_id.clone(),
                option.to_string(),
                value,
            ]);
        }
    }
    if !commands.is_empty() {
        commands.push(";".to_string());
    }
    commands.extend([
        "select-pane".to_string(),
        "-t".to_string(),
        target.pane_id.clone(),
    ]);
    tmux_command_string(&commands)
}

fn is_neovim(command: &str) -> bool {
    matches!(command, "vi" | "vim" | "nvi" | "nvim")
}

fn validate_pane_id(pane_id: &str) -> Result<()> {
    if pane_id.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        Ok(())
    } else {
        bail!("invalid pane ID {pane_id:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(pane_id: &str, left: u32, top: u32, width: u32, height: u32) -> Pane {
        Pane {
            pane_id: pane_id.to_string(),
            pane_pid: pane_id.trim_start_matches('%').parse().unwrap(),
            active: pane_id == "%1",
            cursor_x: 2,
            cursor_y: 3,
            left,
            top,
            width,
            height,
            current_command: "zsh".to_string(),
            sidebar: false,
            floating: false,
        }
    }

    #[test]
    fn chooses_the_nearest_aligned_pane_and_excludes_internal_panes() {
        let source = pane("%1", 40, 0, 20, 20);
        let nearest = pane("%2", 19, 0, 20, 20);
        let farther = pane("%3", 0, 0, 18, 20);
        let mut sidebar = pane("%4", 39, 0, 1, 20);
        sidebar.sidebar = true;
        let mut floating = pane("%5", 38, 0, 1, 20);
        floating.floating = true;
        let panes = vec![source.clone(), farther, sidebar, floating, nearest.clone()];

        assert_eq!(
            choose_target(&panes, &source, PaneSwitchDirection::Left),
            Some((&nearest, false))
        );
    }

    #[test]
    fn cycles_to_the_opposite_aligned_edge() {
        let source = pane("%1", 0, 0, 20, 20);
        let middle = pane("%2", 21, 0, 20, 20);
        let edge = pane("%3", 42, 0, 20, 20);
        let panes = vec![source.clone(), middle, edge.clone()];

        assert_eq!(
            choose_target(&panes, &source, PaneSwitchDirection::Left),
            Some((&edge, true))
        );
    }

    #[test]
    fn neovim_action_sets_cursor_context_before_selecting() {
        let source = pane("%1", 0, 0, 20, 20);
        let mut target = pane("%2", 21, 0, 40, 10);
        target.current_command = "nvim".to_string();

        let action = switch_action(&target, &source, PaneSwitchDirection::Right, false);

        assert!(action.contains("'-p' '-t' '%2' '@vde_nvim_cursor_y' '30'"));
        assert!(action.contains("'-p' '-t' '%2' '@vde_nvim_cursor_x' '0'"));
        assert!(action.contains("'-p' '-t' '%2' '@vde_nvim_select_direction' 'R'"));
        assert!(action.contains("'-p' '-t' '%2' '@vde_nvim_is_cycle' 'false'"));
        assert!(action.contains("'-p' '-t' '%2' '@vde_nvim_target_pane_pid' '2'"));
        assert!(action.ends_with("'select-pane' '-t' '%2'"));
    }
}
