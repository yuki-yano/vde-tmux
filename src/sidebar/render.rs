mod header;
mod rows;
mod text;
mod theme;

pub use header::{
    HeaderAction, HeaderLayout, HeaderLine, HeaderSegment, build_footer_line, build_header_layout,
    build_header_layout_with_counts, build_header_layout_with_theme, header_hit_test,
    render_header_lines,
};
pub use rows::{RenderedLines, WidthTier, render_lines, render_lines_with_indices, render_rows};
pub use theme::SidebarRenderTheme;

pub(crate) use text::{display_width, truncate_display};
