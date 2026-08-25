use super::*;

#[test]
fn display_width_counts_cjk_as_two_cells() {
    assert_eq!(display_width("abc"), 3);
    assert_eq!(display_width("あいう"), 6);
    assert_eq!(display_width("a…"), 2);
}

#[test]
fn truncate_display_appends_ellipsis_within_width() {
    assert_eq!(truncate_display("hello", 10), "hello");
    assert_eq!(truncate_display("hello world", 8), "hello w…");
    assert_eq!(truncate_display("あいうえお", 7), "あいう…");
    assert_eq!(truncate_display("abc", 0), "");
}
