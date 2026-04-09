-- Stock Market Debate — Bull vs Bear with Moderator Synthesis
--
-- Demonstrates the **debate + moderator** pattern: two adversarial agents
-- argue from opposing positions, then a third "moderator" agent reads the
-- transcript and produces a structured JSON synthesis with a recommendation
-- and falsification criteria.
--
-- This pattern is generalizable beyond stocks:
--   - Code review: build_advocate vs critic + synthesizer
--   - Architecture: monolith vs microservices + synthesizer
--   - Product: ship vs delay + synthesizer
--   - Hiring: hire vs pass + synthesizer
--
-- Data is fetched live from Yahoo Finance's public chart endpoint (no API key).
-- Requires: OPENAI_API_KEY (for gpt-5.4-mini)

local TICKER = "NVDA"
local RANGE = "1y"
local DEBATE_TURNS = 6 -- 3 each for bull and bear

print("Fetching " .. TICKER .. " data from Yahoo Finance (" .. RANGE .. ")...")

-- ---------------------------------------------------------------------------
-- 1. Fetch data
-- ---------------------------------------------------------------------------

local resp = http.get(
    "https://query1.finance.yahoo.com/v8/finance/chart/" .. TICKER ..
        "?interval=1d&range=" .. RANGE,
    {
        headers = { ["User-Agent"] = "Mozilla/5.0" },
        timeout = 15,
    }
)

if not resp.ok then
    error("Failed to fetch stock data: HTTP " .. resp.status)
end

local data = resp.json
local result = data.chart.result[1]
local meta = result.meta
local timestamps = result.timestamp or {}
local closes = result.indicators.quote[1].close or {}
local volumes = result.indicators.quote[1].volume or {}

-- Filter out nil values (market holidays may produce them)
local series = {}
for i = 1, #timestamps do
    if closes[i] then
        table.insert(series, {
            ts = timestamps[i],
            close = closes[i],
            volume = volumes[i] or 0,
        })
    end
end

if #series < 50 then
    error("Insufficient data: " .. #series .. " days returned")
end

-- ---------------------------------------------------------------------------
-- 2. Compute analytical metrics
-- ---------------------------------------------------------------------------

local function format_money(n)
    return string.format("$%.2f", n)
end

local function format_pct(n)
    return string.format("%+.1f%%", n)
end

local function mean(arr)
    if #arr == 0 then return 0 end
    local sum = 0
    for _, v in ipairs(arr) do sum = sum + v end
    return sum / #arr
end

local function stddev(arr)
    if #arr < 2 then return 0 end
    local m = mean(arr)
    local sq_sum = 0
    for _, v in ipairs(arr) do sq_sum = sq_sum + (v - m) ^ 2 end
    return math.sqrt(sq_sum / (#arr - 1))
end

local function moving_average(arr, window)
    if #arr < window then return nil end
    local sum = 0
    for i = #arr - window + 1, #arr do sum = sum + arr[i] end
    return sum / window
end

-- Extract close series for math
local close_series = {}
for _, p in ipairs(series) do table.insert(close_series, p.close) end

local current = close_series[#close_series]
local first = close_series[1]
local high_52w = meta.fiftyTwoWeekHigh
local low_52w = meta.fiftyTwoWeekLow

local ma50 = moving_average(close_series, 50)
local ma200 = moving_average(close_series, 200)

-- Daily log returns for volatility
local returns = {}
for i = 2, #close_series do
    local r = math.log(close_series[i] / close_series[i - 1])
    table.insert(returns, r)
end
local daily_vol = stddev(returns)
local annual_vol_pct = daily_vol * math.sqrt(252) * 100

-- Max drawdown over the period
local peak = close_series[1]
local max_dd = 0
for _, c in ipairs(close_series) do
    if c > peak then peak = c end
    local dd = (c - peak) / peak * 100
    if dd < max_dd then max_dd = dd end
end

-- Period returns
local function pct_change(from_idx, to_idx)
    return (close_series[to_idx] - close_series[from_idx]) / close_series[from_idx] * 100
end

local n = #close_series
local r_5d = n >= 6 and pct_change(n - 5, n) or nil
local r_30d = n >= 31 and pct_change(n - 30, n) or nil
local r_90d = n >= 91 and pct_change(n - 90, n) or nil
local r_total = pct_change(1, n)

-- Distance from key levels
local from_high_pct = (current - high_52w) / high_52w * 100
local from_low_pct = (current - low_52w) / low_52w * 100

-- ---------------------------------------------------------------------------
-- 3. Build the data summary that all agents will see
-- ---------------------------------------------------------------------------

local lines = {
    "=== " .. meta.symbol .. " (" .. meta.currency .. ") ===",
    "",
    "Price snapshot:",
    "  Current:        " .. format_money(current),
    "  Previous close: " .. format_money(meta.previousClose or current),
    "  Day range:      " .. format_money(meta.regularMarketDayLow) ..
        " - " .. format_money(meta.regularMarketDayHigh),
    "  52w range:      " .. format_money(low_52w) .. " - " .. format_money(high_52w),
    "",
    "Position vs key levels:",
    "  From 52w high:  " .. format_pct(from_high_pct),
    "  From 52w low:   " .. format_pct(from_low_pct),
}

if ma50 then
    table.insert(lines, "  vs 50d MA:      " .. format_money(ma50) ..
        " (" .. format_pct((current - ma50) / ma50 * 100) .. ")")
end
if ma200 then
    table.insert(lines, "  vs 200d MA:     " .. format_money(ma200) ..
        " (" .. format_pct((current - ma200) / ma200 * 100) .. ")")
end

table.insert(lines, "")
table.insert(lines, "Returns:")
if r_5d then table.insert(lines, "  5-day:   " .. format_pct(r_5d)) end
if r_30d then table.insert(lines, "  30-day:  " .. format_pct(r_30d)) end
if r_90d then table.insert(lines, "  90-day:  " .. format_pct(r_90d)) end
table.insert(lines, "  Period (" .. n .. "d): " .. format_pct(r_total))
table.insert(lines, "")
table.insert(lines, "Risk:")
table.insert(lines, string.format("  Max drawdown (%dd): %.1f%%", n, max_dd))
table.insert(lines, string.format("  Annualized volatility: %.1f%%", annual_vol_pct))

local stock_summary = table.concat(lines, "\n")

print("")
print(stock_summary)
print("")

-- ---------------------------------------------------------------------------
-- 4. Set up the crew with bull, bear, and moderator agents
-- ---------------------------------------------------------------------------

local crew = Crew.new({
    goal = "Bull vs Bear debate on " .. TICKER .. " with moderator synthesis",
    provider = "openai",
    model = "gpt-5.4-mini",
})

crew:add_agent(Agent.new({
    name = "bull",
    goal = "Argue the bull case for " .. TICKER ..
        " with specific data and falsification criteria",
    capabilities = { "analysis", "argumentation" },
    system_prompt = [[
You are a committed bullish equity analyst. You argue the BULL case using
the provided market data: trend, momentum, position relative to moving
averages, drawdown recovery, and 52-week range. Cite specific numbers from
the data.

REQUIRED FORMAT for every turn:
  1. Make your bullish point in 2-3 sentences. Cite at least one number
     from the data.
  2. Address the bear's previous argument directly when one exists.
  3. End with a single line: "INVALIDATION: <specific price level or
     condition that would prove me wrong>"

Do not hedge. Commit to the bullish view. The invalidation level is
mandatory — without it, your turn is incomplete.
]],
}))

crew:add_agent(Agent.new({
    name = "bear",
    goal = "Argue the bear case for " .. TICKER ..
        " with specific data and falsification criteria",
    capabilities = { "analysis", "skepticism" },
    system_prompt = [[
You are a committed bearish equity analyst. You argue the BEAR case using
the provided market data: trend exhaustion, distance from highs, technical
breakdowns, max drawdown, and elevated volatility. Cite specific numbers
from the data.

REQUIRED FORMAT for every turn:
  1. Make your bearish point in 2-3 sentences. Cite at least one number
     from the data.
  2. Address the bull's previous argument directly when one exists.
  3. End with a single line: "INVALIDATION: <specific price level or
     condition that would prove me wrong>"

Do not hedge. Commit to the bearish view. The invalidation level is
mandatory — without it, your turn is incomplete.
]],
}))

crew:add_agent(Agent.new({
    name = "moderator",
    goal = "Read a bull/bear debate and produce a structured actionable synthesis",
    capabilities = { "synthesis", "analysis" },
    system_prompt = [[
You are a senior portfolio manager reviewing a debate between a bullish
and bearish analyst. Read the full transcript and produce a structured
JSON synthesis. Focus on:

  - Facts both sides accept (or implicitly agree on)
  - The actual points of disagreement (not just framing differences)
  - Each side's invalidation level (the price/condition that would prove them wrong)
  - A clear recommendation with confidence level
  - A short rationale that explains why

Be honest about uncertainty. If the debate is genuinely balanced, set
confidence to "low" and recommendation to "hold". Reserve "high" confidence
for cases where one side is clearly on stronger ground.
]],
    response_format = {
        type = "json_schema",
        name = "debate_synthesis",
        schema = {
            type = "object",
            properties = {
                ticker = { type = "string" },
                agreed_facts = {
                    type = "array",
                    items = { type = "string" },
                    description = "Facts both analysts accept",
                },
                key_disagreements = {
                    type = "array",
                    items = { type = "string" },
                    description = "Substantive points of disagreement",
                },
                bull_invalidation = {
                    type = "string",
                    description = "Price level or condition that would invalidate the bull case",
                },
                bear_invalidation = {
                    type = "string",
                    description = "Price level or condition that would invalidate the bear case",
                },
                recommendation = {
                    type = "string",
                    enum = { "buy", "hold", "sell" },
                },
                confidence = {
                    type = "string",
                    enum = { "low", "medium", "high" },
                },
                rationale = {
                    type = "string",
                    description = "1-2 sentences explaining the recommendation",
                },
            },
            required = {
                "ticker",
                "agreed_facts",
                "key_disagreements",
                "bull_invalidation",
                "bear_invalidation",
                "recommendation",
                "confidence",
                "rationale",
            },
            additionalProperties = false,
        },
    },
}))

-- ---------------------------------------------------------------------------
-- 5. Run the bull-vs-bear dialog
-- ---------------------------------------------------------------------------

local debate = crew:dialog({
    agents = { "bull", "bear" },
    starting_speaker = "bull",
    max_turns = DEBATE_TURNS,
    starter = "Here is the recent market data for " .. TICKER .. ":\n\n" ..
        stock_summary .. "\n\n" ..
        "Based on this data, debate whether to BUY " .. TICKER ..
        " at the current level. Bull, make your opening case.",
})

print("=== Debate ===")
print("")
local transcript = debate:run()
for _, turn in ipairs(transcript) do
    print(string.format("--- Turn %d: %s ---", turn.index + 1, string.upper(turn.agent)))
    print(turn.content)
    print("")
end

-- ---------------------------------------------------------------------------
-- 6. Pass the transcript to the moderator for structured synthesis
-- ---------------------------------------------------------------------------

local transcript_text = ""
for _, turn in ipairs(transcript) do
    transcript_text = transcript_text ..
        "[" .. string.upper(turn.agent) .. "]\n" .. turn.content .. "\n\n"
end

local moderator_chat = crew:conversation({
    agent = "moderator",
})

print("=== Moderator Synthesis ===")
print("")

local synthesis_raw = moderator_chat:send(
    "Ticker: " .. TICKER .. "\n\n" ..
    "Market data the analysts used:\n" .. stock_summary .. "\n\n" ..
    "Full debate transcript:\n" .. transcript_text ..
    "Produce the JSON synthesis now."
)

local ok, synthesis = pcall(json_parse, synthesis_raw)
if not ok or not synthesis then
    print("Failed to parse moderator output as JSON. Raw response:")
    print(synthesis_raw)
    return
end

-- Pretty-print the structured synthesis
print("Recommendation: " .. string.upper(synthesis.recommendation) ..
    "  (confidence: " .. synthesis.confidence .. ")")
print("")
print("Rationale: " .. synthesis.rationale)
print("")
print("Bull invalidation: " .. synthesis.bull_invalidation)
print("Bear invalidation: " .. synthesis.bear_invalidation)
print("")

print("Agreed facts:")
for _, f in ipairs(synthesis.agreed_facts) do
    print("  - " .. f)
end
print("")

print("Key disagreements:")
for _, d in ipairs(synthesis.key_disagreements) do
    print("  - " .. d)
end
print("")

print("=== Done ===")
