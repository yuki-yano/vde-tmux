//! tmux コマンド発行層。全機能はこの trait 経由で tmux を呼ぶ
//! (具象 Command 直呼びを禁止し、MockTmuxRunner で全経路をテスト可能にする)。

use anyhow::Result;

#[cfg(test)]
pub mod mock;

pub trait TmuxRunner {
    /// `tmux <args>` を実行し、成功した場合は stdout をそのまま返す。
    fn run(&self, args: &[&str]) -> Result<String>;
}
