local Animal = {} Animal.__index = Animal
function Animal.new(name, sound) return setmetatable({name = name, sound = sound}, Animal) end
function Animal:speak() return self.name .. " says " .. self.sound end
local Dog = setmetatable({}, {__index = Animal}) Dog.__index = Dog
function Dog.new(name) local d = Animal.new(name, "woof") return setmetatable(d, Dog) end
function Dog:fetch() return self.name .. " fetches" end
local d = Dog.new("Rex") print(d:speak(), d:fetch(), getmetatable(d) == Dog, rawget(d, "speak"))
local V = {} V.__index = V
local function vec(x, y) return setmetatable({x = x, y = y}, V) end
V.__add = function(a, b) return vec(a.x + b.x, a.y + b.y) end
V.__sub = function(a, b) return vec(a.x - b.x, a.y - b.y) end
V.__mul = function(a, b) if type(a) == "number" then return vec(a * b.x, a * b.y) elseif type(b) == "number" then return vec(a.x * b, a.y * b) end return a.x * b.x + a.y * b.y end
V.__eq = function(a, b) return a.x == b.x and a.y == b.y end
V.__lt = function(a, b) return a.x < b.x end V.__le = function(a, b) return a.x <= b.x end
V.__tostring = function(a) return "(" .. a.x .. "," .. a.y .. ")" end
V.__len = function() return 2 end V.__unm = function(a) return vec(-a.x, -a.y) end
V.__concat = function(a, b) return tostring(a) .. "|" .. tostring(b) end
V.__call = function(self, k) return self[k] end V.__idiv = function() return "idiv" end V.__mod = function() return "mod" end
local a, b = vec(1, 2), vec(3, 4)
print(tostring(a + b), tostring(b - a), a * b, tostring(2 * a), tostring(a * 2), a == vec(1, 2), a ~= b, a < b, a <= b, a > b, #a, tostring(-a), a .. b, a .. "s", a("x"), a // b, a % b)
local log = {}
local proxy = setmetatable({}, {__index = function(t, k) log[#log + 1] = "get " .. k return k .. "!" end, __newindex = function(t, k, v) log[#log + 1] = "set " .. k rawset(t, k, v) end})
print(proxy.foo) proxy.bar = 1 proxy.bar = 2 print(proxy.bar, table.concat(log, ";"))
local ro = setmetatable({}, {__index = {a = 1}, __newindex = function() error("read-only", 2) end, __metatable = "locked"})
print(ro.a, pcall(function() ro.b = 1 end), getmetatable(ro), pcall(setmetatable, ro, {}))
local counter = setmetatable({}, {__index = function(t, k) return 0 end}) counter.x = counter.x + 1 counter.x = counter.x + 1 print(counter.x, counter.y)
local it = setmetatable({}, {__pairs = function(t) local i = 0 return function() i = i + 1 if i <= 3 then return i, i * i end end, t, nil end})
for k, v in pairs(it) do io.write(k, "=", v, " ") end print()
print(tostring(setmetatable({}, {__tostring = function() return "custom" end})), setmetatable({}, {__name = "MyType"}) ~= nil)
local weak = setmetatable({}, {__mode = "k"}) weak[{}] = 1 collectgarbage() collectgarbage() print(next(weak))
print(pcall(function() return {} + 1 end)) print(pcall(function() return {} < {} end)) print(pcall(function() return #5 end)) print(pcall(function() local n = nil; n() end))
