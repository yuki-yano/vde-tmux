local root = assert(vim.env.VDE_TMUX_NVIM_PLUGIN_ROOT, "VDE_TMUX_NVIM_PLUGIN_ROOT is required")
vim.opt.runtimepath:append(root)

local vde_tmux = require("vde-tmux")
vde_tmux.setup({ keybindings = false })
vim.cmd("vsplit")

local windows = vim.api.nvim_list_wins()
table.sort(windows, function(left, right)
	return vim.api.nvim_win_get_position(left)[2] < vim.api.nvim_win_get_position(right)[2]
end)
assert(#windows == 2, "expected two Neovim windows")
local left_window, right_window = windows[1], windows[2]

local notifications = {}
vim.notify = function(message)
	notifications[#notifications + 1] = message
end
vim.api.nvim_set_current_win(right_window)
vde_tmux.navigate("l")
assert(
	vim.iter(notifications):any(function(message)
		return message:find("pane navigation failed", 1, true) ~= nil
	end),
	"navigation did not invoke the canonical @vde_executable: " .. table.concat(notifications, " | ")
)

local pane_id = assert(vim.env.TMUX_PANE, "TMUX_PANE is required")
local pane_pid = vim.trim(vim.fn.system({ "tmux", "display-message", "-p", "-t", pane_id, "#{pane_pid}" }))
assert(vim.v.shell_error == 0 and pane_pid:match("^[1-9]%d*$"), "failed to resolve pane PID")

for option, value in pairs({
	["@vde_nvim_cursor_y"] = "50",
	["@vde_nvim_cursor_x"] = "90",
	["@vde_nvim_select_direction"] = "R",
	["@vde_nvim_is_cycle"] = "false",
	["@vde_nvim_target_pane_pid"] = pane_pid,
}) do
	local output = vim.fn.system({ "tmux", "set-option", "-p", "-t", pane_id, option, value })
	assert(vim.v.shell_error == 0, output)
end

vim.api.nvim_set_current_win(left_window)
vim.cmd("doautocmd FocusGained")
assert(vim.api.nvim_get_current_win() == right_window, "selection metadata did not choose the right window")

local remaining = vim.trim(vim.fn.system({
	"tmux",
	"display-message",
	"-p",
	"-t",
	pane_id,
	"#{@vde_nvim_cursor_y}#{@vde_nvim_cursor_x}#{@vde_nvim_select_direction}#{@vde_nvim_is_cycle}#{@vde_nvim_target_pane_pid}",
}))
assert(vim.v.shell_error == 0, remaining)
assert(remaining == "", "selection metadata was not cleared")

vim.cmd("quitall!")
