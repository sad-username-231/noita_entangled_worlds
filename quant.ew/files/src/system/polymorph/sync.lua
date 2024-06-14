local util = dofile_once("mods/quant.ew/files/src/util.lua")
local ctx = dofile_once("mods/quant.ew/files/src/ctx.lua")
local net = dofile_once("mods/quant.ew/files/src/net.lua")
local player_fns = dofile_once("mods/quant.ew/files/src/player_fns.lua")
local np = require("noitapatcher")

local rpc = net.new_rpc_namespace()

local module = {}

local function entity_changed()
    local currently_polymorphed = not EntityHasTag(ctx.my_player.entity, "player_unit")

    ctx.my_player.currently_polymorphed = currently_polymorphed
end

function module.on_world_update_post()
    local ent = np.GetPlayerEntity()
    if ent ~= ctx.my_player.entity then
        player_fns.replace_player_entity(ent, ctx.my_player)
        EntityAddTag(ent, "ew_no_enemy_sync")
        entity_changed()
    end
end

function module.on_projectile_fired(shooter_id, projectile_id, initial_rng, position_x, position_y, target_x, target_y, send_message,
    unknown1, multicast_index, unknown3)
    if ctx.my_player.currently_polymorphed and shooter_id == ctx.my_player.entity then
        local projectileComponent = EntityGetFirstComponentIncludingDisabled(projectile_id, "ProjectileComponent")
        local entity_that_shot    = ComponentGetValue2(projectileComponent, "mEntityThatShot")
        GamePrint("Shot "..entity_that_shot)
        if entity_that_shot == 0 then
            GamePrint("Sending projectile")
            local x, y = EntityGetTransform(projectile_id)
            rpc.replicate_projectile(x, y, np.SerializeEntity(projectile_id))
        end
    end
    EntityAddTag(projectile_id, "ew_replicated")
end

function rpc.replicate_projectile(x, y, seri_ent)
    local ent = EntityCreateNew()
    np.DeserializeEntity(ent, seri_ent, x, y)
    EntityAddTag(ent, "ew_no_enemy_sync")
end

return module
