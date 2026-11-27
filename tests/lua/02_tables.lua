local t = {5, 2, 8, 1, 9, 3}
table.sort(t) print(table.concat(t, ","))
table.sort(t, function(a, b) return a > b end) print(table.concat(t, ","))
local words = {"pear", "Apple", "fig", "banana"}
table.sort(words, function(a, b) return a:lower() < b:lower() end) print(table.concat(words, " "))
table.insert(t, 7) table.insert(t, 1, 0) print(table.concat(t, ","), table.remove(t), table.remove(t, 1), #t)
print(table.unpack({1, 2, 3})) print(table.unpack({1, 2, 3}, 2)) print(table.pack(1, nil, 3).n, select("#", table.unpack({1, nil, 3}, 1, 3)))
print(table.concat(table.move({1, 2, 3}, 1, 3, 2), ","), table.concat({}, ","), table.concat({1, 2.5, "x"}, "-", 2, 3))
local keys = {} for k in pairs({a = 1, b = 2, c = 3}) do keys[#keys + 1] = k end table.sort(keys) print(table.concat(keys))
local s = 0 for i, v in ipairs({1, 2, 3, nil, 5}) do s = s + v end print(s)
local grid = {} for y = 1, 3 do grid[y] = {} for x = 1, 3 do grid[y][x] = x * y end end print(grid[2][3], #grid, #grid[1])
local m = {} m[1.0] = "a" m[2^53] = "b" print(m[1], m[2^53], next(m) ~= nil)
local big = {} for i = 1, 10000 do big[i] = i end for i = 1, 10000, 2 do big[i] = nil end local c = 0 for _ in pairs(big) do c = c + 1 end print(c)
local q = {first = 1, last = 0} function q.push(v) q.last = q.last + 1 q[q.last] = v end function q.pop() local v = q[q.first] q[q.first] = nil q.first = q.first + 1 return v end
q.push("a") q.push("b") print(q.pop(), q.pop(), q.pop())
print(#"", #{n = 1}, {} == {}, next({}, nil), type(table.unpack), pcall(table.concat, {{}}))
local stable = {} for i = 1, 50 do stable[i] = {k = i % 5, i = i} end table.sort(stable, function(a, b) if a.k ~= b.k then return a.k < b.k end return a.i < b.i end) print(stable[1].i, stable[11].i, stable[50].i)
print(pcall(table.sort, {3, 1, "x"}), pcall(table.insert, {}, 5, 1))
