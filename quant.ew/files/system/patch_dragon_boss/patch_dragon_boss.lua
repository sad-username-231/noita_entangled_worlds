ModLuaFileAppend("data/scripts/buildings/dragonspot.lua", "mods/quant.ew/files/system/patch_dragon_boss/dragonspot_script.lua")
util.replace_text_in("data/entities/buildings/dragonspot.xml", "player_unit", "ew_peer")

local module = {}

return module