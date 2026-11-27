local function gen(n) return coroutine.wrap(function() for i = 1, n do coroutine.yield(i) end end) end
for v in gen(4) do io.write(v, " ") end print()
local co = coroutine.create(function(a, b) local c = coroutine.yield(a + b) local d, e = coroutine.yield(c * 2) return d + e end)
print(coroutine.resume(co, 1, 2)) print(coroutine.resume(co, 10)) print(coroutine.resume(co, 3, 4)) print(coroutine.resume(co), coroutine.status(co))
local co2 = coroutine.create(function() error("oops") end) print(coroutine.resume(co2)) print(coroutine.status(co2))
local co3 = coroutine.create(function() local ok, err = pcall(function() coroutine.yield(1) error("inner") end) coroutine.yield(ok, err) return "end" end)
print(coroutine.resume(co3)) print(coroutine.resume(co3)) print(coroutine.resume(co3))
print(coroutine.isyieldable(), type(coroutine.running()), select(2, coroutine.running()))
local function producer() return coroutine.create(function() for _, w in ipairs({"a", "b", "c"}) do coroutine.yield(w) end end) end
local function filter(p) return coroutine.create(function() local n = 0 while true do local ok, v = coroutine.resume(p) if not v then break end n = n + 1 coroutine.yield(n .. ":" .. v) end end) end
local f = filter(producer()) while true do local ok, v = coroutine.resume(f) if not v then break end io.write(v, " ") end print()
local w = coroutine.wrap(function() return 1 end) w() print(pcall(w))
local tasks, out = {}, {}
for id = 1, 3 do tasks[id] = coroutine.create(function() for step = 1, id do out[#out + 1] = id .. "." .. step coroutine.yield() end end) end
local alive = true while alive do alive = false for _, t in ipairs(tasks) do if coroutine.status(t) == "suspended" then coroutine.resume(t) alive = true end end end print(table.concat(out, " "))
print(select("#", coroutine.resume(coroutine.create(function(...) return ... end), 1, nil, nil)))
local c4 = coroutine.wrap(function() for i = 1, 3 do coroutine.yield(table.sort({3, 1, 2}, function(a, b) return a < b end) or i) end end) print(c4(), c4(), c4())
print(coroutine.close(coroutine.create(function() end)), pcall(coroutine.yield))
