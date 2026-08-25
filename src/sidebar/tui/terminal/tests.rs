use super::*;

#[test]
fn panic_restore_emits_mouse_disable_and_alternate_screen_exit() {
    let mut output = Vec::new();

    restore_terminal_after_panic(&mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("\u{1b}[?1000l"), "{output:?}");
    assert!(output.contains("\u{1b}[?1049l"), "{output:?}");
}
