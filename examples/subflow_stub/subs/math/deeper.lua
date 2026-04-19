-- Co-located with math.lua. A sub-flow calling `run_flow("deeper.lua")`
-- from math.lua should resolve against math.lua's directory
-- (examples/subflow_stub/subs/math/), NOT the parent crew.lua dir.
return { where = "deeper", x = (input and input.x or 0) }
