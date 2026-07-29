--- @diagnostic disable:undefined-global

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
	env = { TAU_ENCRYPTION_KEY = random_key() }
})

local resp = dstest.http(s, "GET", "/healthz")
assert(resp.status == 200, "/healthz should return 200")
dstest.info("Health check passed")

local metrics = dstest.http(s, "GET", "/metrics")
assert(metrics.status == 200, "/metrics should return 200")
dstest.info("Metrics endpoint OK")

local result = dstest.step()
dstest.info("Injected fault: " .. result.fault)

if result.fault ~= "pause" and result.fault ~= "kill" then
	local ok, r = pcall(dstest.http, s, "GET", "/healthz", { port = 9100 })
	if ok and r.status == 200 then
		dstest.info("Container healthy after fault")
	else
		dstest.warn("Container degraded after fault")
	end
end

dstest.clear(s)
dstest.info("Test complete")
