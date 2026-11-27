-- a deterministic tetris-like simulation and a small text adventure state machine
local seed = 12345
local function rnd(n) seed = (seed * 1103515245 + 12345) % 2147483648 return seed % n + 1 end
local W, H = 10, 20
local shapes = {{{1,1,1,1}}, {{1,1},{1,1}}, {{0,1,0},{1,1,1}}, {{1,0,0},{1,1,1}}, {{0,1,1},{1,1,0}}}
local board = {} for y = 1, H do board[y] = {} for x = 1, W do board[y][x] = 0 end end
local function rotate(s) local r = {} for x = 1, #s[1] do r[x] = {} for y = #s, 1, -1 do r[x][#r[x] + 1] = s[y][x] end end return r end
local function fits(s, px, py) for y = 1, #s do for x = 1, #s[y] do if s[y][x] == 1 then local bx, by = px + x - 1, py + y - 1 if bx < 1 or bx > W or by > H or (by >= 1 and board[by][bx] ~= 0) then return false end end end end return true end
local function place(s, px, py, id) for y = 1, #s do for x = 1, #s[y] do if s[y][x] == 1 and py + y - 1 >= 1 then board[py + y - 1][px + x - 1] = id end end end end
local function clear() local n = 0 for y = H, 1, -1 do local full = true for x = 1, W do if board[y][x] == 0 then full = false break end end if full then table.remove(board, y) table.insert(board, 1, {}) for x = 1, W do board[1][x] = 0 end n = n + 1 end end return n end
local score, pieces, lines = 0, 0, 0
while pieces < 200 do
  local id = rnd(#shapes) local s = shapes[id] for _ = 1, rnd(4) - 1 do s = rotate(s) end
  local px, py = rnd(W - #s[1] + 1), 0
  if not fits(s, px, py + 1) then break end
  while fits(s, px, py + 1) do py = py + 1 end
  place(s, px, py, id) pieces = pieces + 1
  local n = clear() lines = lines + n score = score + ({0, 40, 100, 300, 1200})[n + 1]
end
local filled = 0 for y = 1, H do for x = 1, W do if board[y][x] ~= 0 then filled = filled + 1 end end end
print(pieces, lines, score, filled)
for y = H - 4, H do local row = {} for x = 1, W do row[x] = board[y][x] == 0 and "." or tostring(board[y][x]) end print(table.concat(row)) end
local State = {} State.__index = State
local rooms = {hall = {n = "library", e = "kitchen"}, library = {s = "hall"}, kitchen = {w = "hall", d = "cellar"}, cellar = {u = "kitchen"}}
local p = setmetatable({room = "hall", inv = {}, moves = 0}, State)
function State:go(d) local r = rooms[self.room][d] if r then self.room = r self.moves = self.moves + 1 return true end return false, "no exit " .. d end
for d in ("n s e d u w x n"):gmatch("%a") do local ok, err = p:go(d) io.write(d, ok and "+" or "-", " ") end print(p.room, p.moves)
local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end
local memo = setmetatable({}, {__index = function(t, n) local v = n < 2 and n or t[n - 1] + t[n - 2] t[n] = v return v end})
print(fib(20), memo[80], string.format("%.6f", (function() local s = 0 for i = 1, 100000 do s = s + math.sin(i) * math.cos(i / 3) end return s end)()))
local function deep(n) if n == 0 then return 0 end return 1 + deep(n - 1) end print(deep(5000), select("#", table.unpack({}, 1, 200)))
print(pcall(function() local function inf(n) return inf(n + 1) + 1 end return inf(1) end))
