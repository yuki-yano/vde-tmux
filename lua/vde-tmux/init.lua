local M = {}

local defaults = {
	enable = true,
	keybindings = {
		left = "<C-h>",
		down = "<C-j>",
		up = "<C-k>",
		right = "<C-l>",
	},
	modes = { "n", "t" },
	debug = false,
	disable_when_floating = true,
	navigate_from_floating = true,
}

local direction_names = {
	h = "left",
	j = "down",
	k = "up",
	l = "right",
}

local vim_directions = {
	left = "h",
	down = "j",
	up = "k",
	right = "l",
}

local selection_options = {
	cursor_y = "@vde_nvim_cursor_y",
	cursor_x = "@vde_nvim_cursor_x",
	direction = "@vde_nvim_select_direction",
	is_cycle = "@vde_nvim_is_cycle",
	target_pid = "@vde_nvim_target_pane_pid",
}

local executable_option = "@vde_executable"
local context_separator = "\31"

local state = {
	config = vim.deepcopy(defaults),
	in_tmux = vim.env.TMUX ~= nil,
	pending_selection = vim.env.TMUX ~= nil,
}

local function debug_log(message, ...)
	if state.config.debug then
		vim.notify(string.format("[vde-tmux] " .. message, ...), vim.log.levels.DEBUG)
	end
end

local function normal_windows()
	local windows = {}
	for _, window in ipairs(vim.api.nvim_list_wins()) do
		if vim.api.nvim_win_get_config(window).relative == "" then
			local position = vim.api.nvim_win_get_position(window)
			local width = vim.api.nvim_win_get_width(window)
			local height = vim.api.nvim_win_get_height(window)
			windows[#windows + 1] = {
				window = window,
				row = position[1],
				col = position[2],
				width = width,
				height = height,
				bottom = position[1] + height,
				right = position[2] + width,
			}
		end
	end
	return windows
end

local function is_floating(window)
	return vim.api.nvim_win_get_config(window).relative ~= ""
end

local function notify_error(message)
	vim.notify("vde-tmux: " .. message, vim.log.levels.ERROR)
end

local function current_pane_context()
	local pane_id = vim.env.TMUX_PANE
	if type(pane_id) ~= "string" or not pane_id:match("^%%%d+$") then
		return nil, nil, nil, "TMUX_PANE does not contain a valid pane ID"
	end
	local context = vim.trim(vim.fn.system({
		"tmux",
		"display-message",
		"-p",
		"-t",
		pane_id,
		"#{pane_pid}" .. context_separator .. "#{" .. executable_option .. "}",
	}))
	if vim.v.shell_error ~= 0 then
		return nil, nil, nil, "failed to resolve the current tmux pane context"
	end
	local fields = vim.split(context, context_separator, { plain = true })
	local pane_pid = fields[1] or ""
	local executable = fields[2] or ""
	if not pane_pid:match("^[1-9]%d*$") then
		return nil, nil, nil, "failed to resolve the current tmux pane PID"
	end
	if not executable:match("^/") then
		return nil, nil, nil, "tmux does not contain an absolute @vde_executable"
	end
	if vim.fn.executable(executable) ~= 1 then
		return nil, nil, nil, "@vde_executable is not executable: " .. executable
	end
	return pane_id, pane_pid, executable, nil
end

local function navigate_to_tmux(direction)
	local direction_name = direction_names[direction]
	if direction_name == nil then
		notify_error("invalid navigation direction: " .. tostring(direction))
		return
	end
	local pane_id, pane_pid, executable, context_error = current_pane_context()
	if context_error ~= nil then
		notify_error(context_error)
		return
	end
	debug_log("pane-switch %s from %s/%s", direction_name, pane_id, pane_pid)
	local result = vim.fn.system({
		executable,
		"pane-switch",
		direction_name,
		"--pane-id",
		pane_id,
		"--pane-pid",
		pane_pid,
	})
	if vim.v.shell_error ~= 0 then
		notify_error("pane navigation failed: " .. vim.trim(result))
	end
end

local function is_at_edge(direction)
	return #normal_windows() <= 1 or vim.fn.winnr() == vim.fn.winnr(direction)
end

local function cycle_window(windows, direction, target_row, target_col)
	local best_window = nil
	local best_edge = nil
	local best_distance = math.huge
	for _, bounds in ipairs(windows) do
		local edge
		local distance
		if direction == "U" then
			edge = bounds.bottom
			distance = math.abs((bounds.col + bounds.width / 2) - target_col)
			if best_edge == nil or edge > best_edge or (edge == best_edge and distance < best_distance) then
				best_window, best_edge, best_distance = bounds.window, edge, distance
			end
		elseif direction == "D" then
			edge = bounds.row
			distance = math.abs((bounds.col + bounds.width / 2) - target_col)
			if best_edge == nil or edge < best_edge or (edge == best_edge and distance < best_distance) then
				best_window, best_edge, best_distance = bounds.window, edge, distance
			end
		elseif direction == "L" then
			edge = bounds.right
			distance = math.abs((bounds.row + bounds.height / 2) - target_row)
			if best_edge == nil or edge > best_edge or (edge == best_edge and distance < best_distance) then
				best_window, best_edge, best_distance = bounds.window, edge, distance
			end
		elseif direction == "R" then
			edge = bounds.col
			distance = math.abs((bounds.row + bounds.height / 2) - target_row)
			if best_edge == nil or edge < best_edge or (edge == best_edge and distance < best_distance) then
				best_window, best_edge, best_distance = bounds.window, edge, distance
			end
		end
	end
	return best_window
end

local function select_window(cursor_y_percent, cursor_x_percent, direction, is_cycle)
	local windows = normal_windows()
	if #windows == 0 then
		return
	end
	if #windows == 1 then
		vim.api.nvim_set_current_win(windows[1].window)
		return
	end

	local target_row = math.floor(cursor_y_percent * (vim.o.lines - vim.o.cmdheight) / 100)
	local target_col = math.floor(cursor_x_percent * vim.o.columns / 100)
	if is_cycle then
		local target = cycle_window(windows, direction, target_row, target_col)
		if target ~= nil then
			vim.api.nvim_set_current_win(target)
			return
		end
	end

	local best_window = nil
	local best_distance = math.huge
	for _, bounds in ipairs(windows) do
		if
			target_row >= bounds.row
			and target_row < bounds.bottom
			and target_col >= bounds.col
			and target_col < bounds.right
		then
			vim.api.nvim_set_current_win(bounds.window)
			return
		end
		local distance = math.abs(target_row - (bounds.row + bounds.height / 2))
			+ math.abs(target_col - (bounds.col + bounds.width / 2))
		if distance < best_distance then
			best_window, best_distance = bounds.window, distance
		end
	end
	if best_window ~= nil then
		vim.api.nvim_set_current_win(best_window)
	end
end

local function selection_metadata(pane_id)
	local separator = string.char(31)
	local format = table.concat({
		"#{pane_pid}",
		"#{" .. selection_options.cursor_y .. "}",
		"#{" .. selection_options.cursor_x .. "}",
		"#{" .. selection_options.direction .. "}",
		"#{" .. selection_options.is_cycle .. "}",
		"#{" .. selection_options.target_pid .. "}",
	}, separator)
	local output = vim.fn.system({ "tmux", "display-message", "-p", "-t", pane_id, format })
	if vim.v.shell_error ~= 0 then
		return nil
	end
	output = output:gsub("[\r\n]+$", "")
	local fields = {}
	for field in (output .. separator):gmatch("(.-)" .. separator) do
		fields[#fields + 1] = field
	end
	if #fields ~= 6 then
		return nil
	end
	return {
		pane_pid = fields[1],
		cursor_y = fields[2],
		cursor_x = fields[3],
		direction = fields[4],
		is_cycle = fields[5],
		target_pid = fields[6],
	}
end

local function clear_selection_metadata(pane_id)
	local arguments = { "tmux" }
	for _, option in pairs(selection_options) do
		if #arguments > 1 then
			arguments[#arguments + 1] = ";"
		end
		vim.list_extend(arguments, { "set-option", "-pu", "-t", pane_id, option })
	end
	vim.fn.system(arguments)
end

local function process_selection()
	if not state.in_tmux or not state.pending_selection then
		return
	end
	state.pending_selection = false
	local pane_id = vim.env.TMUX_PANE
	if type(pane_id) ~= "string" or not pane_id:match("^%%%d+$") then
		return
	end
	local metadata = selection_metadata(pane_id)
	if metadata == nil or metadata.target_pid == "" then
		return
	end

	local cursor_y = tonumber(metadata.cursor_y)
	local cursor_x = tonumber(metadata.cursor_x)
	local direction = metadata.direction
	local is_cycle = metadata.is_cycle
	local valid = cursor_y ~= nil
		and cursor_y >= 0
		and cursor_y <= 100
		and cursor_x ~= nil
		and cursor_x >= 0
		and cursor_x <= 100
		and direction ~= nil
		and direction:match("^[LRUD]$") ~= nil
		and (is_cycle == "true" or is_cycle == "false")
		and metadata.pane_pid:match("^[1-9]%d*$") ~= nil
		and metadata.target_pid == metadata.pane_pid
	if not valid then
		clear_selection_metadata(pane_id)
		notify_error("tmux pane selection metadata is invalid")
		return
	end
	if state.config.disable_when_floating and is_floating(vim.api.nvim_get_current_win()) then
		clear_selection_metadata(pane_id)
		return
	end
	select_window(cursor_y, cursor_x, direction, is_cycle == "true")
	clear_selection_metadata(pane_id)
end

function M.navigate(direction)
	if direction_names[direction] == nil then
		notify_error("invalid navigation direction: " .. tostring(direction))
		return
	end
	if not state.in_tmux then
		vim.cmd("wincmd " .. direction)
		return
	end
	if state.config.navigate_from_floating and is_floating(vim.api.nvim_get_current_win()) then
		navigate_to_tmux(direction)
	elseif is_at_edge(direction) then
		navigate_to_tmux(direction)
	else
		vim.cmd("wincmd " .. direction)
	end
end

function M.setup(config)
	state.config = vim.tbl_deep_extend("force", vim.deepcopy(defaults), config or {})
	state.in_tmux = vim.env.TMUX ~= nil
	state.pending_selection = state.in_tmux
	vim.g.vde_tmux_nvim_configured = true
	if not state.config.enable then
		return
	end

	if state.config.keybindings then
		for direction, key in pairs(state.config.keybindings) do
			local vim_direction = vim_directions[direction]
			if key and vim_direction then
				vim.keymap.set(state.config.modes, key, function()
					M.navigate(vim_direction)
				end, { silent = true, desc = "Navigate " .. direction })
			end
		end
	end

	local group = vim.api.nvim_create_augroup("VdeTmuxNavigation", { clear = true })
	if state.in_tmux then
		vim.api.nvim_create_autocmd("FocusLost", {
			group = group,
			callback = function()
				state.pending_selection = true
			end,
			desc = "Mark vde-tmux pane selection as pending",
		})
		vim.api.nvim_create_autocmd("FocusGained", {
			group = group,
			callback = process_selection,
			desc = "Restore the target Neovim window after a tmux pane switch",
		})
	end
end

function M.get_config()
	return state.config
end

return M
