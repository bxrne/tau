local rows = tau.range('returns', 0, 1000000)
local n, sum, sum2 = 0, 0.0, 0.0
for _, r in ipairs(rows) do
  n = n + 1
  sum = sum + r.v
  sum2 = sum2 + r.v * r.v
end
if n < 2 then
  tau.log('need at least 2 return samples, got ' .. n)
  return
end
local mean = sum / n
local variance = sum2 / n - mean * mean
local sd = math.sqrt(math.max(variance, 0))
local sh = (sd > 0) and (mean / sd) or 0.0
tau.log(string.format('sharpe: n=%d mean=%.6f sd=%.6f ratio=%.4f', n, mean, sd, sh))
tau.exec(('APPEND LENS sharpe 0 1 %f'):format(sh))
