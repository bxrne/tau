--- @diagnostic disable: undefined-global

package.path = "dst/?.lua;" .. package.path

local core = require("core")

dstest.config({
	substrate = "docker",
	seed = 42,
})

local id = core.spawn()
core.assert_health(id)

local conn = core.connect(id)
local faults = core.faults_new()
local ok_stmt = core.expect_ok(faults)
local not_err = core.expect_not_err(faults)
local val_f = core.expect_val_float(faults)
local expect_nil = core.expect_nil(faults)
local expect_names = core.expect_names(faults)

local function send(stmt)
	conn:send(stmt .. "\n")
	return conn:recv_line():gsub("[\r\n]", "")
end

ok_stmt(conn, "AUTH admin changeme_use_a_strong_password")
ok_stmt(conn, "CREATE DATABASE sensors")
ok_stmt(conn, "CREATE LENS celsius float")
ok_stmt(conn, "APPEND LENS celsius 0 3600 18.0")
ok_stmt(conn, "DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0")

local c_resp = send("AT LENS celsius 1800")
local c_val = val_f("AT LENS celsius 1800", c_resp, 18.0)
dstest.info(string.format("I1  celsius @ 1800 = %.1f C", c_val))

local f_resp = send("AT LENS fahrenheit 1800")
local f_val = val_f("AT LENS fahrenheit 1800", f_resp, 64.4, 1e-6)
dstest.info(string.format("I2  fahrenheit @ 1800 = %.1f F", f_val))

assert(math.abs(f_val - (c_val * 9.0 / 5.0 + 32.0)) < 1e-9,
	"conversion formula violated: F=" .. tostring(f_val) .. " C=" .. tostring(c_val))
dstest.info("I3  F = C * 9/5 + 32  holds")

local show = send("SHOW LENSES")
expect_names("SHOW LENSES", show, { "celsius", "fahrenheit" })
dstest.info("I4  SHOW LENSES -> " .. show)

local miss = send("AT LENS celsius 5000")
expect_nil("AT LENS celsius 5000", miss)
dstest.info("I5  celsius @ 5000 -> VAL NIL")

local fmiss = send("AT LENS fahrenheit 5000")
expect_nil("AT LENS fahrenheit 5000", fmiss)
dstest.info("I6  fahrenheit @ 5000 -> VAL NIL")

local bye = send("QUIT")
assert(bye == "OK BYE", "expected OK BYE, got: " .. bye)
conn:close()
dstest.info("disconnected: " .. bye)

core.assert_zero_faults(faults)
