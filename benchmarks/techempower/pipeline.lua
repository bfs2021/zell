-- wrk -s pipeline.lua ... -- <depth>: sends <depth> requests per write and
-- reads as many responses (HTTP/1.1 pipelining, as TechEmpower's plaintext test).
init = function(args)
  local depth = tonumber(args[1]) or 1
  local r = {}
  for i = 1, depth do r[i] = wrk.format() end
  req = table.concat(r)
end

request = function()
  return req
end
