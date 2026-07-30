--- @diagnostic disable: undefined-global

package.path = "dst/?.lua;" .. package.path

local core = require("core")

dstest.config({
	substrate = "docker",
	seed = 42,
})

local id = core.spawn()

core.assert_health(id)
core.assert_metrics(id)

local result = core.step_and_check(id)

core.cleanup(id)
