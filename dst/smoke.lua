--- @diagnostic disable: undefined-global

dstest.config({
	substrate = "docker",
	seed = 42,
})

local function random_key()
	local key = ""
	for _ = 1, 32 do
		key = key .. string.char(math.random(0, 255))
	end
	return key
end

local s = dstest.setup("docker", {
	image = "ghcr.io/bxrne/tau:latest",
	ports = { 9100, 7070 },
	volumes = { "/home/bxrne/Projects/tau/container/tau-config.toml:/data/tau-config.toml" },
	env = { TAU_ENCRYPTION_KEY = random_key() },
})

-- wait for server: dstest.http has built-in retries
dstest.http(s, "GET", "/healthz")
dstest.info("server healthy")

-- connect
local conn, err = dstest.tcp(s, 7070)
assert(conn, "tcp connect failed: " .. tostring(err))
conn:set_timeout(5)
dstest.info("connected to " .. conn:addr())

local faults = 0

local function cmd(stmt)
	conn:send(stmt .. "\n")
	local line = conn:recv_line()
	assert(line, "connection closed after: " .. stmt)
	return (line:gsub("[\r\n]", ""))
end

local function assert_ok(stmt)
	local resp = cmd(stmt)
	if resp:sub(1, 3) == "ERR" then
		faults = faults + 1
		error(string.format("%s -> %s", stmt, resp))
	end
	assert(resp == "OK", string.format("expected OK, got: %s (after %q)", resp, stmt))
	dstest.info(string.format("%-45s -> %s", stmt, resp))
	return resp
end

local function assert_not_err(stmt, resp)
	if resp:sub(1, 3) == "ERR" then
		faults = faults + 1
		error(string.format("%s -> %s", stmt, resp))
	end
end

assert_ok("AUTH admin changeme_use_a_strong_password")
assert_ok("CREATE DATABASE sensors")
assert_ok("CREATE LENS celsius float")
assert_ok("APPEND LENS celsius 0 3600 18.0")
assert_ok("DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0")

-- I1: celsius point lookup returns the appended float
local c_resp = cmd("AT LENS celsius 1800")
assert_not_err("AT LENS celsius 1800", c_resp)
assert(c_resp:sub(1, 5) == "VAL f", "expected VAL f..., got: " .. c_resp)
local c_val = tonumber(c_resp:match("VAL f(.+)"))
assert(c_val ~= nil, "could not parse celsius value from: " .. c_resp)
assert(math.abs(c_val - 18.0) < 1e-9, "celsius should be 18.0, got: " .. tostring(c_val))
dstest.info(string.format("I1  celsius @ 1800 = %.1f C", c_val))

-- I2: fahrenheit point lookup returns the derived float
local f_resp = cmd("AT LENS fahrenheit 1800")
assert_not_err("AT LENS fahrenheit 1800", f_resp)
assert(f_resp:sub(1, 5) == "VAL f", "expected VAL f..., got: " .. f_resp)
local f_val = tonumber(f_resp:match("VAL f(.+)"))
assert(f_val ~= nil, "could not parse fahrenheit value from: " .. f_resp)
assert(math.abs(f_val - 64.4) < 1e-6, "fahrenheit should be 64.4, got: " .. tostring(f_val))
dstest.info(string.format("I2  fahrenheit @ 1800 = %.1f F", f_val))

-- I3: conversion formula holds exactly: F = C * 9/5 + 32
assert(math.abs(f_val - (c_val * 9.0 / 5.0 + 32.0)) < 1e-9,
	"conversion formula violated: F=" .. tostring(f_val) .. " C=" .. tostring(c_val))
dstest.info("I3  F = C * 9/5 + 32  holds")

-- I4: SHOW LENSES lists both celsius and fahrenheit
local show = cmd("SHOW LENSES")
assert_not_err("SHOW LENSES", show)
assert(show:sub(1, 5) == "NAMES", "expected NAMES..., got: " .. show)
assert(show:find("celsius", 1, true), "celsius not in: " .. show)
assert(show:find("fahrenheit", 1, true), "fahrenheit not in: " .. show)
dstest.info("I4  SHOW LENSES -> " .. show)

-- I5: point lookup outside the data range returns NIL
local miss = cmd("AT LENS celsius 5000")
assert_not_err("AT LENS celsius 5000", miss)
assert(miss == "VAL NIL", "expected VAL NIL out of range, got: " .. miss)
dstest.info("I5  celsius @ 5000 -> VAL NIL (out of range)")

-- I6: fahrenheit is also NIL outside the source range
local fmiss = cmd("AT LENS fahrenheit 5000")
assert_not_err("AT LENS fahrenheit 5000", fmiss)
assert(fmiss == "VAL NIL", "expected VAL NIL out of range, got: " .. fmiss)
dstest.info("I6  fahrenheit @ 5000 -> VAL NIL (out of range)")


local bye = cmd("QUIT")
assert(bye == "OK BYE", "expected OK BYE, got: " .. bye)
conn:close()
dstest.info("disconnected: " .. bye)


assert(faults == 0, string.format("%d fault(s) during session", faults))
dstest.info("all invariants passed — 0 faults")
