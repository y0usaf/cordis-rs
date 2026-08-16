-- Cordis in Lua: revertible effects + reactive coeffects

local count = 0
local d = ctx:effect(function()
  count = count + 1
  return function() count = count - 1 end
end, "counter")
print("after effect, count =", count)
d()
print("after dispose, count =", count)

local started = false
local f = ctx:plugin(function(c, cfg)
  started = true
  return function() started = false end
end, { foo = "bar" })
print("plugin state:", f:state(), "started:", started)
f:dispose()
print("after dispose, started:", started)

local runs = 0
ctx:inject({"svc"}, function(c)
  runs = runs + 1
  print("consumer ran, svc =", c.svc)
end)
print("runs before provide:", runs)
local pd = ctx:provide("svc", 42)
print("runs after provide:", runs)
pd()
print("runs after unprovide:", runs)

local got = 0
local od = ctx:on("hello", function(x) got = got + x end)
ctx:emit("hello", 5)
print("got:", got)
od()

print("demo done")
