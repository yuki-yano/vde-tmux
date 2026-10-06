//! Claude Code reads image files named in a bracketed paste asynchronously and discards an Enter
//! that arrives during that read. Prompts that name images are therefore submitted only after the
//! paste has reached Claude's input field.

use std::time::Duration;

use crate::detect::is_claude_prompt_separator;

/// Upper bound for Claude to read and resize the pasted images.
pub(super) const IMAGE_PASTE_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

const IMAGE_EXTENSIONS: [&str; 5] = [".png", ".jpg", ".jpeg", ".gif", ".webp"];
const PASTING_INDICATOR: &str = "Pasting…";

/// Mirrors Claude's paste segmentation: split at newlines and at a space before an absolute path,
/// then treat a segment as an image path when its unquoted, unescaped text ends with an image
/// extension. Claude takes the asynchronous path whether or not the file exists.
pub(super) fn reads_image_paths(prompt: &str) -> bool {
    prompt
        .split('\n')
        .flat_map(path_segments)
        .any(is_image_path)
}

/// The paste has reached Claude's input field when the composer differs from the pre-paste screen
/// and the footer no longer reports an in-flight paste. Claude inserts the paste and clears its
/// pending-Enter state in the same update, so an Enter sent afterwards is processed.
pub(super) fn paste_settled(before: &str, now: &str) -> bool {
    let (Some((composer_before, _)), Some((composer_now, footer))) =
        (composer(before), composer(now))
    else {
        return false;
    };
    composer_now != composer_before && !footer.iter().any(|line| line.trim() == PASTING_INDICATOR)
}

fn path_segments(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    for index in 0..bytes.len() {
        if bytes[index] == b' ' && starts_absolute_path(&bytes[index + 1..]) {
            segments.push(&line[start..index]);
            start = index + 1;
        }
    }
    segments.push(&line[start..]);
    segments
}

fn starts_absolute_path(rest: &[u8]) -> bool {
    match rest {
        [b'/', ..] => true,
        [drive, b':', b'\\', ..] => drive.is_ascii_alphabetic(),
        _ => false,
    }
}

fn is_image_path(segment: &str) -> bool {
    let trimmed = segment.trim();
    let unquoted = ['"', '\'']
        .into_iter()
        .find_map(|quote| trimmed.strip_prefix(quote)?.strip_suffix(quote))
        .unwrap_or(trimmed);
    let mut unescaped = String::with_capacity(unquoted.len());
    let mut chars = unquoted.chars();
    while let Some(ch) = chars.next() {
        unescaped.push(if ch == '\\' {
            chars.next().unwrap_or('\\')
        } else {
            ch
        });
    }
    let unescaped = unescaped.to_ascii_lowercase();
    IMAGE_EXTENSIONS
        .iter()
        .any(|extension| unescaped.ends_with(extension))
}

/// Returns Claude's composer (the latest `❯` line through the next separator) and the lines below.
fn composer(screen: &str) -> Option<(Vec<&str>, Vec<&str>)> {
    let lines = screen.lines().map(str::trim_end).collect::<Vec<_>>();
    let start = lines.iter().rposition(|line| line.starts_with('❯'))?;
    let end = lines[start..]
        .iter()
        .position(|line| is_claude_prompt_separator(line))
        .map_or(lines.len(), |offset| start + offset);
    Some((lines[start..end].to_vec(), lines[end..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_image_paths_that_claude_reads_on_paste() {
        for prompt in [
            "describe this\n/tmp/shot.png",
            "look at /tmp/shot.PNG",
            "compare /tmp/a.jpg /tmp/b.jpeg",
            "'/tmp/my shot.webp'",
            "/tmp/my\\ shot.gif",
            "see C:\\Users\\me\\shot.png",
            "/tmp/missing.png",
        ] {
            assert!(reads_image_paths(prompt), "{prompt:?}");
        }
    }

    #[test]
    fn ignores_image_names_that_claude_keeps_as_text() {
        for prompt in [
            "look at /tmp/shot.png please",
            "/tmp/shot.png.txt",
            "rename shot.png to icon.svg",
            "fix the image loader",
        ] {
            assert!(!reads_image_paths(prompt), "{prompt:?}");
        }
    }

    const FOOTER: &str = "  ⏵⏵ auto mode on (shift+tab to cycle)";
    const PASTING: &str = "  Pasting…";

    fn screen(composer: &str, footer: &str) -> String {
        format!("❯ earlier prompt\n⏺ done\n────────\n❯\u{a0}{composer}\n────────\n{footer}\n")
    }

    #[test]
    fn paste_is_unsettled_while_claude_reads_images() {
        let idle = screen("", FOOTER);
        assert!(!paste_settled(&idle, &idle));
        assert!(!paste_settled(&idle, &screen("", PASTING)));
        assert!(!paste_settled(
            &idle,
            &screen("[Image #1]describe this", PASTING)
        ));
    }

    #[test]
    fn paste_settles_once_the_composer_changes() {
        let idle = screen("", FOOTER);
        assert!(paste_settled(
            &idle,
            &screen("[Image #1]describe this", FOOTER)
        ));
        assert!(paste_settled(
            &idle,
            &screen("[Image #2]\n  and more", FOOTER)
        ));
    }

    #[test]
    fn paste_never_settles_without_a_composer() {
        assert!(!paste_settled("no prompt\n", "still no prompt\n"));
    }
}
