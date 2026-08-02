--- @diagnostic disable: undefined-global
--- oracle.lua — exhaustive invariant + scenario verification under fault
--- injection. Spins up one Tau container, builds a dataset exercising every
--- read path, then runs the dstest oracle: predicates (per-round) and
--- invariants (continuous) across all six fault types.
--- Run: dstest < dst/oracle.lua

local IMAGE   = "ghcr.io/bxrne/tau:latest"
local M_PORT  = 29100
local P_PORT  = 27070
local AUTH    = "AUTH admin changeme_use_a_strong_password"

local function random_key()
	local k = {}
	for _ = 1, 32 do k[#k + 1] = string.format("%02x", math.random(0, 255)) end
	return table.concat(k)
end

local function write_config(path)
	local f = assert(io.open(path, "w"))
	f:write('bind = "0.0.0.0:' .. P_PORT .. '"\n')
	f:write('log_level = "info"\n')
	f:write("compact_threshold = 8\n\n")
	f:write("[wal]\nenabled = true\npath = \"/data/tau.wal\"\n\n")
	f:write("[tls]\nenabled = false\n\n")
	f:write("[auth]\nenabled = true\nusername = \"admin\"\n")
	f:write("password = \"changeme_use_a_strong_password\"\n")
	f:write("users_file = \"/data/users.db\"\n\n")
	f:write("[metrics]\nport = " .. M_PORT .. "\n\n")
	f:write("[limits]\nmax_connections = 1024\nidle_timeout_secs = 300\n")
	f:close()
	return path
end

local function spawn()
	local cfg = write_config(string.format("/tmp/tau-oracle-%d.toml", M_PORT))
	return dstest.setup({
		image = IMAGE,
		ports = { M_PORT, P_PORT },
		volumes = { cfg .. ":/data/tau-config.toml:ro" },
		env = { TAU_ENCRYPTION_KEY = random_key() },
	})
end

local function connect(s)
	local conn, err = dstest.net.tcp(s, P_PORT)
	if not conn then return nil, err end
	conn:set_timeout(5)
	return conn
end

local function query(s, stmt)
	local conn, err = connect(s)
	if not conn then return nil, err end
	conn:send(stmt .. "\n")
	local line = conn:recv_line()
	conn:close()
	if not line then return nil, "closed" end
	return (line:gsub("[\r\n]", ""))
end

local function query_authed(s, stmt)
	local conn, err = connect(s)
	if not conn then return nil, err end
	conn:send(AUTH .. "\n")
	conn:recv_line()
	conn:send("USE DATABASE sensors\n")
	conn:recv_line()
	conn:send(stmt .. "\n")
	local line = conn:recv_line()
	conn:close()
	if not line then return nil, "closed" end
	return (line:gsub("[\r\n]", ""))
end

local function parse_float(resp)
	return tonumber(resp:match("VAL f(.+)"))
end

local function fatal(fault)
	return fault == "pause" or fault == "kill" or fault == "deprive:network"
end

dstest.config({
	substrate = "docker",
	seed = 42,
	weights = {
		pause = 0.22,
		kill = 0.22,
		["deprive:disk"] = 0.0,
		["deprive:network"] = 0.18,
		["deprive:memory"] = 0.19,
		["deprive:cpu"] = 0.19,
	},
	accumulation = "single",
	http_timeout = 8,
	http_retries = 40,
	http_retry_delay = 300,
	step_delay = 800,
})

local s = spawn()
dstest.info("spawned " .. s)

local hresp = dstest.net.http(s, "GET", "/healthz")
assert(hresp.status == 200, "initial /healthz != 200")
dstest.info("health: /healthz -> 200")

dstest.info("building dataset")
local c = connect(s)
assert(c, "setup connect failed")
c:send(AUTH .. "\n"); assert(c:recv_line():sub(1, 2) == "OK", "AUTH failed")
c:send("CREATE DATABASE sensors\n");      assert(c:recv_line():sub(1, 2) == "OK")
c:send("CREATE LENS celsius float\n");     assert(c:recv_line():sub(1, 2) == "OK")
c:send("APPEND LENS celsius 0 3600 18.0\n"); assert(c:recv_line():sub(1, 2) == "OK")
c:send("APPEND LENS celsius 0 1200 20.0\n"); assert(c:recv_line():sub(1, 2) == "OK")
c:send("DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0\n"); assert(c:recv_line():sub(1, 2) == "OK")
c:send("DERIVE LENS daytime AS celsius OVER 0 2400\n"); assert(c:recv_line():sub(1, 2) == "OK")
c:send("CREATE LENS requests int\n");     assert(c:recv_line():sub(1, 2) == "OK")
c:send("BATCH APPEND LENS requests { 0 60 50 ; 60 120 99 }\n"); assert(c:recv_line():sub(1, 2) == "OK")
c:send("APPEND LENS requests 0 60 45\n"); assert(c:recv_line():sub(1, 2) == "OK")
c:close()
dstest.info("dataset ready: celsius(+correction), fahrenheit, daytime(bounded), requests(+correction)")

dstest.dst.oracle.predicate("health", function(subject, fault, round)
	if fatal(fault) then return true end
	local ok, r = pcall(dstest.net.http, subject, "GET", "/healthz")
	if not ok or r.status ~= 200 then
		return { false, string.format("/healthz %s (round %d)", tostring(r), round) }
	end
	return true
end)

dstest.dst.oracle.predicate("point_lookup", function(subject, fault, round)
	if fatal(fault) then return true end
	local r = query_authed(subject, "AT LENS celsius 1800")
	if not r or not r:find("VAL f18", 1, true) then
		return { false, "AT celsius 1800 = " .. tostring(r) }
	end
	local r2 = query_authed(subject, "AT LENS celsius 30")
	if not r2 or not r2:find("VAL f20", 1, true) then
		return { false, "AT celsius 30 (newest-wins) = " .. tostring(r2) }
	end
	local r3 = query_authed(subject, "AT LENS celsius 5000")
	if r3 ~= "VAL NIL" then
		return { false, "AT celsius 5000 expected NIL, got " .. tostring(r3) }
	end
	return true
end)

dstest.dst.oracle.predicate("derived_lens", function(subject, fault, round)
	if fatal(fault) then return true end
	local r = query_authed(subject, "AT LENS fahrenheit 1800")
	local v = r and parse_float(r)
	if not v or math.abs(v - 64.4) > 1e-6 then
		return { false, "AT fahrenheit 1800 = " .. tostring(r) }
	end
	local r2 = query_authed(subject, "AT LENS fahrenheit 30")
	local v2 = r2 and parse_float(r2)
	if not v2 or math.abs(v2 - 68.0) > 1e-6 then
		return { false, "AT fahrenheit 30 = " .. tostring(r2) }
	end
	local r3 = query_authed(subject, "AT LENS fahrenheit 5000")
	if r3 ~= "VAL NIL" then
		return { false, "AT fahrenheit 5000 expected NIL, got " .. tostring(r3) }
	end
	return true
end)

dstest.dst.oracle.predicate("bounded_derive", function(subject, fault, round)
	if fatal(fault) then return true end
	local r = query_authed(subject, "AT LENS daytime 1800")
	if not r or not r:find("VAL f18", 1, true) then
		return { false, "AT daytime 1800 (in-bounds) = " .. tostring(r) }
	end
	local r2 = query_authed(subject, "AT LENS daytime 3000")
	if r2 ~= "VAL NIL" then
		return { false, "AT daytime 3000 (out-of-bounds) expected NIL, got " .. tostring(r2) }
	end
	return true
end)

dstest.dst.oracle.predicate("newest_wins", function(subject, fault, round)
	if fatal(fault) then return true end
	local r = query_authed(subject, "AT LENS requests 30")
	if not r or not r:find("VAL i45", 1, true) then
		return { false, "AT requests 30 (corrected) = " .. tostring(r) }
	end
	local r2 = query_authed(subject, "AT LENS requests 90")
	if not r2 or not r2:find("VAL i99", 1, true) then
		return { false, "AT requests 90 (uncorrected) = " .. tostring(r2) }
	end
	return true
end)

dstest.dst.oracle.predicate("range_shape", function(subject, fault, round)
	if fatal(fault) then return true end
	local r = query_authed(subject, "RANGE LENS celsius 0 3600")
	if not r or r:sub(1, 5) ~= "RANGE" then
		return { false, "RANGE celsius 0 3600 = " .. tostring(r) }
	end
	local n = tonumber(r:match("RANGE (%d+)"))
	if not n or n < 2 then
		return { false, "RANGE expected >=2 segments, got " .. tostring(r) }
	end
	if not r:find("0:1200:f20", 1, true) then
		return { false, "RANGE missing corrected segment 0:1200:f20: " .. r }
	end
	return true
end)

dstest.dst.oracle.predicate("reduce_agg", function(subject, fault, round)
	if fatal(fault) then return true end
	local r = query_authed(subject, "REDUCE LENS requests 0 120 USING sum")
	if not r or not r:find("VAL i144", 1, true) then
		return { false, "REDUCE sum = " .. tostring(r) }
	end
	local r2 = query_authed(subject, "REDUCE LENS requests 0 120 USING count")
	if not r2 or not r2:find("VAL i2", 1, true) then
		return { false, "REDUCE count = " .. tostring(r2) }
	end
	local r3 = query_authed(subject, "REDUCE LENS requests 0 120 USING max")
	if not r3 or not r3:find("VAL i99", 1, true) then
		return { false, "REDUCE max = " .. tostring(r3) }
	end
	return true
end)

local prev_was_kill = false
dstest.dst.oracle.predicate("wal_durability", function(subject, fault, round)
	if fault == "kill" then
		prev_was_kill = true
		return true
	end
	if fault == "pause" or fault == "deprive:network" then
		prev_was_kill = false
		return true
	end
	if prev_was_kill then
		prev_was_kill = false
		local r = query_authed(subject, "AT LENS celsius 1800")
		if not r or not r:find("VAL f18", 1, true) then
			return { false, "WAL replay after kill failed: AT celsius 1800 = " .. tostring(r) }
		end
		local r2 = query_authed(subject, "AT LENS requests 90")
		if not r2 or not r2:find("VAL i99", 1, true) then
			return { false, "WAL replay after kill failed: AT requests 90 = " .. tostring(r2) }
		end
		dstest.info(string.format("round %d: WAL replay verified after kill", round))
	end
	return true
end)

dstest.dst.oracle.invariant("metrics_up", function()
	local ok, r = pcall(dstest.net.http, s, "GET", "/metrics")
	if not ok then return true end
	if r.status ~= 200 then
		return { false, "/metrics returned " .. tostring(r.status) }
	end
	return true
end)

dstest.dst.oracle.invariant("conversion_identity", function()
	local rc = query_authed(s, "AT LENS celsius 1800")
	local rf = query_authed(s, "AT LENS fahrenheit 1800")
	local cv = rc and parse_float(rc)
	local fv = rf and parse_float(rf)
	if cv and fv then
		if math.abs(fv - (cv * 9.0 / 5.0 + 32.0)) > 1e-6 then
			return { false, string.format("F=%.4f != C*9/5+32=%.4f", fv, cv * 9.0 / 5.0 + 32.0) }
		end
	end
	return true
end)

dstest.info("running oracle experiment: 12 fault rounds across all fault types")
local report = dstest.dst.oracle.run(function()
	local results = dstest.dst.run_steps(12)
	for _, r in ipairs(results) do
		dstest.info(string.format("round %d: %s on %s", r.round, r.fault, r.subject))
	end
end)

dstest.info(string.format(
	"report: passed=%s  total=%d  passed=%d  failed=%d",
	tostring(report.passed), report.total_checks, report.passed_checks, report.failed_checks
))

if not report.passed then
	dstest.error("oracle failures:")
	for _, f in ipairs(report.failures) do
		dstest.error(string.format("  [%s] %s: %s", f.type, f.name, f.error))
	end
end

dstest.dst.clear(s)
assert(report.passed, "oracle verification failed")
dstest.info("oracle: PASS — all invariants and scenarios held under fault injection")
