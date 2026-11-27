print(1, 1.0, -0.0, 1/3, 10//3, 10.0//3, 7 % 3, -7 % 3, 7 % -3, 2^10, 1e15, 1e16, 2^53, 0x10, 1e100)
print(3 == 3.0, math.type(3), math.type(3.0), math.type("3"), 3 | 4, 7 & 2, 1 << 62, 1 << 63, ~0, 5 ~ 3)
print(math.maxinteger + 1 == math.mininteger, math.maxinteger // -1, 5 // 0.0, -5 // 0.0, 0/0 ~= 0/0)
print(tostring(nil), tostring(true), tonumber("0x1p4"), tonumber("  12  "), tonumber("1e2"), tonumber("abc"), tonumber("10", 2), tonumber("zz", 36))
print(math.tointeger(3.0), math.tointeger(3.5), math.floor(-3.5), math.ceil(-3.5), math.abs(-4), math.max(1, 5, 3), math.min(2, -1))
print(string.format("%5.2f|%-5d|%05d|%x|%X|%o|%e|%g|%g|%s|%q|%c|%%|%10s|%-10s|", 3.14159, 42, 42, 255, 255, 8, 12345.678, 1e20, 0.1, "hi", "a\nb", 65, "r", "l"))
print(string.format("%d %s %s %.3f %5s %i", 3.0, 1.5, {} ~= nil, 2/3, true, -7))
print(#"héllo", ("x"):rep(3, ","), ("hello"):sub(2, -2), ("hello"):upper(), ("Hello"):lower(), ("abc"):reverse(), ("abc"):byte(1, -1))
print(string.char(72, 105), ("  trim  "):match("^%s*(.-)%s*$"), ("a,b,,c"):find(",", 1, true), ("hello world"):find("o w"), ("x=1, y=2"):match("(%w+)=(%w+)"))
local a, b, c = (function() return 1, 2, 3 end)()
print(a, b, c, select("#", 1, nil, 3), select(2, "a", "b", "c"), select(-1, "a", "b", "c"))
local t = {10, 20, 30, nil, 50}
print(#{1, 2, 3}, t[5], next({}), type(next), rawlen({1, 2}), rawequal(t, t), rawget(t, 1))
print(10 == "10", "10" + 5, "3" * "4", 10 .. 20, "a" < "b", "Z" < "a", 1 < 2.5, "10" + 0 == 10)
goto done
print("skipped")
::done::
for i = 10, 1, -3 do io.write(i, " ") end print()
for i = 1.0, 2.0, 0.5 do io.write(i, " ") end print()
for i = 1, 3 do for j = 1, 3 do if j == 2 then goto continue end io.write(i, j, " ") ::continue:: end end print()
local x <const> = 5
do local y <close> = setmetatable({}, {__close = function() print("closed") end}) end
print(x, type(print), type(nil), type({}), type("s"), type(2), type(true), type(coroutine.create(print)))
print(pcall(error, "msg"), pcall(error, {code = 1}), select(2, pcall(error)), pcall(function() local z = nil; return z.x end))
print(select(2, pcall(function() error("lvl", 0) end)), select(2, pcall(function() error("lvl2") end)), select(2, xpcall(function() error("h") end, function(m) return "handled: " .. m end)))
print(load("return 1 + 1")(), load("syntax error here"), type((load(function() return nil end))))
print(string.format("%s %s", 1e15, 2^63), math.pi, -math.pi, 100, 100.0, 1e308 * 10, -1e308 * 10, 2^-1074)
print(8 // 3, 8.0 // 3, -8 // 3, 8 % 3.5, 2^0.5, 10 / 2, 3 * 1.0, 1e2, .5, 5 // 1, 7 // 2 * 2 + 7 % 2)
