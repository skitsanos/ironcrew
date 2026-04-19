-- Simple arithmetic sub-flow used by tests. Reads `input.x`, returns
-- `{ got = x + 1 }`. Does not touch the LLM.
return { got = (input and input.x or 0) + 1 }
