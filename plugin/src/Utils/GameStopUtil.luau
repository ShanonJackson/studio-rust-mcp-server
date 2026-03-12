local Main = script:FindFirstAncestor("MCPStudioPlugin")
local PluginUtils = require(Main.Utils.PluginUtils)
local StudioTestService = game:GetService("StudioTestService")

local MCP_STOP_PLAY_MESSAGE_KEY = "MCP_STOP_PLAY_MESSAGE"
local MCP_STOP_COMPLETED_KEY = "MCP_STOP_COMPLETED"

local function stopPlay()
	PluginUtils.setSettings(MCP_STOP_COMPLETED_KEY, false)
	PluginUtils.setSettings(MCP_STOP_PLAY_MESSAGE_KEY, true)
end

local function isStopCompleted(): boolean
	return PluginUtils.getSettings(MCP_STOP_COMPLETED_KEY) == true
end

local function monitorForStopPlay()
	PluginUtils.setSettings(MCP_STOP_PLAY_MESSAGE_KEY, false)
	PluginUtils.setSettings(MCP_STOP_COMPLETED_KEY, false)
	while true do
		if PluginUtils.getSettings(MCP_STOP_PLAY_MESSAGE_KEY) then
			PluginUtils.setSettings(MCP_STOP_PLAY_MESSAGE_KEY, false)
			StudioTestService:EndTest({})
			PluginUtils.setSettings(MCP_STOP_COMPLETED_KEY, true)
		end
		task.wait(0.1)
	end
end

return { stopPlay = stopPlay, monitorForStopPlay = monitorForStopPlay, isStopCompleted = isStopCompleted }
