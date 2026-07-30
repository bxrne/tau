--- @diagnostic disable: undefined-global

package.path = "dst/?.lua;" .. package.path

local core = require("core")

dstest.config({
	substrate = "docker",
	seed = 42,
})

local function smoke_setup(id, M, ports)
	local conn = M.connect(id, ports.protocol_port)
	local ok = M.expect_ok(M.faults_new())
	ok(conn, "AUTH admin changeme_use_a_strong_password")
	ok(conn, "CREATE DATABASE sensors")
	ok(conn, "CREATE LENS celsius float")
	ok(conn, "APPEND LENS celsius 0 3600 18.0")
	ok(conn, "DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0")
	conn:close()
end

local function health_check(id, fault_result, M)
	if fault_result.fault == "pause"
		or fault_result.fault == "kill"
		or fault_result.fault == "deprive:network"
	then
		return
	end
	local ok, resp = pcall(dstest.http, id, "GET", "/healthz")
	if not ok or resp.status ~= 200 then
		error(string.format("unhealthy after %s (round %d)", fault_result.fault, fault_result.round))
	end
end

local configs = {
	{ name = "baseline",     spawn_opts = nil },
	{ name = "debug-on",      spawn_opts = { env = { TAU_LOG_LEVEL = "debug" } } },
	{ name = "short-timeout", spawn_opts = { env = { TAU_REQUEST_TIMEOUT = "1" } } },
}

local specs = {}
for _, cfg in ipairs(configs) do
	specs[#specs + 1] = {
		name = cfg.name,
		spawn_opts = cfg.spawn_opts,
		setup = smoke_setup,
		check = health_check,
	}
end

local report = core.orchestrate(specs, { rounds = 10 })
assert(report.failed == 0, string.format("%d experiment(s) failed", report.failed))
dstest.info("sweep complete")
