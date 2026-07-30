--- @diagnostic disable: undefined-global
-- core.lua
-- Shared helpers distilled from alive.lua and smoke.lua, plus a
-- coroutine-based experiment orchestrator for table-driven sweeps.

local M = {}

M.IMAGE = "ghcr.io/bxrne/tau:latest"
M.PORTS = { 9100, 7070 }
M.VOLUMES = { "/home/bxrne/Projects/tau/container/tau-config.toml:/data/tau-config.toml" }

function M.random_key()
	local key = ""
	for _ = 1, 32 do
		key = key .. string.char(math.random(0, 255))
	end
	return key
end

M.BASE_CONFIG = "/home/bxrne/Projects/tau/container/tau-config.toml"
M._next_port = 20000

function M.unique_ports()
	local m = M._next_port
	local p = M._next_port + 1
	M._next_port = M._next_port + 2
	return m, p
end

function M.write_config(metrics_port, protocol_port)
	local path = string.format("/tmp/tau-dstest-%d.toml", metrics_port)
	local f = io.open(path, "w")
	f:write(string.format('bind = "0.0.0.0:%d"\n', protocol_port))
	f:write("log_level = \"info\"\n")
	f:write("compact_threshold = 8\n\n")
	f:write("[wal]\nenabled = true\npath = \"/data/tau.wal\"\n\n")
	f:write("[tls]\nenabled = false\n\n")
	f:write("[auth]\nenabled = true\nusername = \"admin\"\n")
	f:write("password = \"changeme_use_a_strong_password\"\n")
	f:write("users_file = \"/data/users.db\"\n\n")
	f:write(string.format("[metrics]\nport = %d\n\n", metrics_port))
	f:write("[limits]\nmax_connections = 1024\nidle_timeout_secs = 300\n")
	f:close()
	return path
end

function M.defaults()
	return {
		image = M.IMAGE,
		ports = M.PORTS,
		volumes = { M.BASE_CONFIG .. ":/data/tau-config.toml" },
		env = { TAU_ENCRYPTION_KEY = M.random_key() },
	}
end

function M.spawn(overrides)
	local opts = M.defaults()
	if overrides then
		for k, v in pairs(overrides) do
			opts[k] = v
		end
	end
	local id = dstest.setup("docker", opts)
	return id
end

function M.spawn_unique(overrides)
	local m_port, p_port = M.unique_ports()
	local cfg_path = M.write_config(m_port, p_port)
	local opts = {
		image = M.IMAGE,
		ports = { m_port, p_port },
		volumes = { cfg_path .. ":/data/tau-config.toml" },
		env = { TAU_ENCRYPTION_KEY = M.random_key() },
	}
	if overrides then
		for k, v in pairs(overrides) do
			if k == "env" then
				for ek, ev in pairs(v) do
					opts.env[ek] = ev
				end
			else
				opts[k] = v
			end
		end
	end
	return M.spawn(opts), m_port, p_port
end

function M.assert_health(id)
	local resp = dstest.http(id, "GET", "/healthz")
	assert(resp.status == 200, "/healthz should return 200")
	dstest.info("Health check passed for " .. id)
	return resp
end

function M.assert_metrics(id)
	local resp = dstest.http(id, "GET", "/metrics")
	assert(resp.status == 200, "/metrics should return 200")
	dstest.info("Metrics endpoint OK for " .. id)
	return resp
end

function M.connect(id, port)
	port = port or 7070
	local conn, err = dstest.tcp(id, port)
	assert(conn, "tcp connect failed: " .. tostring(err))
	conn:set_timeout(5)
	dstest.info("connected to " .. conn:addr())
	return conn
end

local function cmd(conn, stmt)
	conn:send(stmt .. "\n")
	local line = conn:recv_line()
	assert(line, "connection closed after: " .. stmt)
	return (line:gsub("[\r\n]", ""))
end

local function assert_ok(conn, stmt, faults)
	local resp = cmd(conn, stmt)
	if resp:sub(1, 3) == "ERR" then
		faults.n = faults.n + 1
		error(string.format("%s -> %s", stmt, resp))
	end
	assert(resp == "OK", string.format("expected OK, got: %s (after %q)", resp, stmt))
	dstest.info(string.format("%-45s -> %s", stmt, resp))
	return resp
end

local function assert_not_err(stmt, resp, faults)
	if resp:sub(1, 3) == "ERR" then
		faults.n = faults.n + 1
		error(string.format("%s -> %s", stmt, resp))
	end
end

function M.faults_new()
	return { n = 0 }
end

function M.expect_ok(faults)
	return function(conn, stmt)
		return assert_ok(conn, stmt, faults)
	end
end

function M.expect_not_err(faults)
	return function(stmt, resp)
		return assert_not_err(stmt, resp, faults)
	end
end

function M.assert_zero_faults(faults)
	assert(faults.n == 0, string.format("%d fault(s) during session", faults.n))
	dstest.info("all invariants passed — 0 faults")
end

function M.parse_float(resp)
	local val = tonumber(resp:match("VAL f(.+)"))
	assert(val ~= nil, "could not parse float from: " .. resp)
	return val
end

function M.expect_val_float(faults)
	return function(stmt, resp, expected, epsilon)
		epsilon = epsilon or 1e-9
		assert_not_err(stmt, resp, faults)
		assert(resp:sub(1, 5) == "VAL f", "expected VAL f..., got: " .. resp)
		local val = M.parse_float(resp)
		assert(math.abs(val - expected) < epsilon,
			string.format("%s: expected %.6f, got %.6f", stmt, expected, val))
		dstest.info(string.format("  %s = %.3f", stmt, val))
		return val
	end
end

function M.expect_nil(faults)
	return function(stmt, resp)
		assert_not_err(stmt, resp, faults)
		assert(resp == "VAL NIL", "expected VAL NIL, got: " .. resp)
		dstest.info("  " .. stmt .. " -> VAL NIL")
	end
end

function M.expect_names(faults)
	return function(stmt, resp, expected_names)
		assert_not_err(stmt, resp, faults)
		assert(resp:sub(1, 5) == "NAMES", "expected NAMES..., got: " .. resp)
		for _, name in ipairs(expected_names) do
			assert(resp:find(name, 1, true), name .. " not in: " .. resp)
		end
		dstest.info("  " .. stmt .. " -> ok (" .. #expected_names .. " names)")
	end
end

function M.step_and_check(id, on_safe)
	local result = dstest.step()
	dstest.info("Injected fault: " .. result.fault)
	if result.fault ~= "pause" and result.fault ~= "kill" then
		if on_safe then
			on_safe(result)
		else
			local ok, r = pcall(dstest.http, id, "GET", "/healthz")
			if ok and r.status == 200 then
				dstest.info("Container healthy after fault")
			else
				dstest.warn("Container degraded after fault")
			end
		end
	end
	return result
end

function M.cleanup(id)
	dstest.clear(id)
	dstest.info("Test complete")
end

function M.run_suite(setup_fn, steps_fn)
	local id = setup_fn()
	local ok, err = pcall(steps_fn, id)
	dstest.clear(id)
	if not ok then
		dstest.error("suite failed: " .. tostring(err))
		return false, err
	end
	return true
end

function M.orchestrate(specs, opts)
	opts = opts or {}
	local rounds = opts.rounds or 10
	local n = #specs

	local ids = {}
	local threads = {}
	local results = {}

	for i, spec in ipairs(specs) do
		local id, m_port, p_port
		if spec.spawn then
			id = spec.spawn()
		else
			id, m_port, p_port = M.spawn_unique(spec.spawn_opts)
		end
		ids[i] = id
		threads[i] = coroutine.create(function()
			if spec.setup then
				spec.setup(id, M, { metrics_port = m_port, protocol_port = p_port })
			end
			local ok = true
			local err
			while true do
				local fault_result = coroutine.yield(ok)
				if fault_result == "done" then break end
				if not fault_result or not fault_result.more then break end
				if spec.check and ok then
					local cok, cerr = pcall(spec.check, id, fault_result, M)
					if not cok then
						ok = false
						err = cerr
						dstest.warn(string.format("[%s] check failed round %d: %s",
							spec.name, fault_result.round, tostring(cerr)))
					end
				end
			end
			if spec.teardown then
				pcall(spec.teardown, id, M)
			else
				dstest.clear(id)
			end
			results[i] = { name = spec.name, ok = ok, err = err }
		end)
	end

	for _, co in ipairs(threads) do
		if coroutine.status(co) ~= "dead" then
			coroutine.resume(co)
		end
	end

	for _ = 1, rounds do
		local sok, sresult = pcall(dstest.step)
		if not sok then
			dstest.warn("fault injection error: " .. tostring(sresult))
			break
		end
		if not sresult.more then break end
		for _, co in ipairs(threads) do
			if coroutine.status(co) ~= "dead" then
				coroutine.resume(co, sresult)
			end
		end
	end

	for _, co in ipairs(threads) do
		if coroutine.status(co) ~= "dead" then
			coroutine.resume(co, "done")
		end
	end

	local passed, failed = 0, 0
	for _, r in ipairs(results) do
		if r.ok then
			passed = passed + 1
			dstest.info(string.format("PASS  %s", r.name))
		else
			failed = failed + 1
			dstest.error(string.format("FAIL  %s  %s", r.name, tostring(r.err)))
		end
	end

	dstest.info(string.format("orchestrate: %d/%d passed (%d failed)", passed, n, failed))
	return { passed = passed, failed = failed, total = n, results = results }
end

return M
