--- @diagnostic disable: undefined-global
--- smoke.lua — basic verification that the Tau container image, protocol, and
--- query plumbing work end-to-end. No fault injection; just "does it ork".
--- Run: dstest < dst/smoke.lua

local IMAGE   = "ghcr.io/bxrne/tau:latest"
local M_PORT  = 19100
local P_PORT  = 17070
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
	local cfg = write_config(string.format("/tmp/tau-smoke-%d.toml", M_PORT))
	return dstest.setup({
		image = IMAGE,
		ports = { M_PORT, P_PORT },
		volumes = { cfg .. ":/data/tau-config.toml:ro" },
		env = { TAU_ENCRYPTION_KEY = random_key() },
	})
end

local function assert_health(s)
	local resp = dstest.net.http(s, "GET", "/healthz")
	assert(resp.status == 200, "expected /healthz 200, got " .. tostring(resp.status))
	dstest.info("health: /healthz -> 200")
end

local function connect(s)
	local conn, err = dstest.net.tcp(s, P_PORT)
	assert(conn, "tcp connect failed: " .. tostring(err))
	conn:set_timeout(5)
	return conn
end

local function send(conn, stmt)
	conn:send(stmt .. "\n")
	local line = conn:recv_line()
	assert(line, "connection closed after: " .. stmt)
	return (line:gsub("[\r\n]", ""))
end

local function expect_ok(conn, stmt)
	local r = send(conn, stmt)
	assert(r:sub(1, 2) == "OK", string.format("expected OK, got %q (after %q)", r, stmt))
	dstest.info(string.format("  %-52s -> %s", stmt, r))
end

local function expect_float(conn, stmt, want, eps)
	eps = eps or 1e-9
	local r = send(conn, stmt)
	assert(r:sub(1, 5) == "VAL f", "expected VAL f..., got: " .. r)
	local v = assert(tonumber(r:match("VAL f(.+)")), "parse float: " .. r)
	assert(math.abs(v - want) < eps, string.format("%s: want %.6f got %.6f", stmt, want, v))
	dstest.info(string.format("  %-52s -> %s", stmt, r))
	return v
end

local function expect_nil(conn, stmt)
	local r = send(conn, stmt)
	assert(r == "VAL NIL", "expected VAL NIL, got: " .. r)
	dstest.info(string.format("  %-52s -> %s", stmt, r))
end

local function expect_names(conn, stmt, want)
	local r = send(conn, stmt)
	assert(r:sub(1, 5) == "NAMES", "expected NAMES..., got: " .. r)
	for _, n in ipairs(want) do
		assert(r:find(n, 1, true), n .. " not in: " .. r)
	end
	dstest.info(string.format("  %-52s -> %s", stmt, r))
end

dstest.config({ substrate = "docker", seed = 42, http_retries = 40, http_retry_delay = 300 })

local s = spawn()
dstest.info("spawned " .. s)
assert_health(s)

local c = connect(s)
dstest.info("connected to " .. c:addr())

expect_ok(c, AUTH)
expect_ok(c, "CREATE DATABASE sensors")
expect_ok(c, "CREATE LENS celsius float")
expect_ok(c, "APPEND LENS celsius 0 3600 18.0")
expect_ok(c, "DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0")

local cv = expect_float(c, "AT LENS celsius 1800", 18.0)
local fv = expect_float(c, "AT LENS fahrenheit 1800", 64.4, 1e-6)
assert(math.abs(fv - (cv * 9.0 / 5.0 + 32.0)) < 1e-9, "F = C*9/5+32 violated")
dstest.info("conversion identity F = C*9/5+32 holds")

expect_names(c, "SHOW LENSES", { "celsius", "fahrenheit" })
expect_nil(c, "AT LENS celsius 5000")
expect_nil(c, "AT LENS fahrenheit 5000")

local bye = send(c, "QUIT")
assert(bye == "OK BYE", "expected OK BYE, got: " .. bye)
c:close()
dstest.info("disconnected: " .. bye)

dstest.dst.clear(s)
dstest.info("smoke: PASS — container orks")
