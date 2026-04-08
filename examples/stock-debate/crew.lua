-- Stock Market Debate — Bull vs Bear
--
-- Demonstrates agent-to-agent dialog (crew:dialog) with real market data
-- fetched from Yahoo Finance via the http global. The bull and bear analysts
-- debate over multiple turns based on the same data snapshot.
--
-- No API key required — Yahoo Finance's public chart endpoint is open access.
-- Requires: OPENAI_API_KEY (for gpt-5.4-mini)

local TICKER = "NVDA"
local RANGE = "1mo"

print("Fetching " .. TICKER .. " data from Yahoo Finance...")

-- Fetch live market data
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

-- Build a compact data summary for the agents
local function format_money(n)
    return string.format("$%.2f", n)
end

local lines = {
    string.format("Ticker: %s (%s)", meta.symbol, meta.currency),
    string.format("Current price: %s", format_money(meta.regularMarketPrice)),
    string.format("Day range: %s - %s", format_money(meta.regularMarketDayLow), format_money(meta.regularMarketDayHigh)),
    string.format("52-week range: %s - %s", format_money(meta.fiftyTwoWeekLow), format_money(meta.fiftyTwoWeekHigh)),
    string.format("Exchange: %s | Timezone: %s", meta.exchangeName, meta.timezone),
    "",
    "Recent daily closes:",
}

local first_close, last_close
for i = 1, #timestamps do
    local close = closes[i]
    if close then
        if not first_close then first_close = close end
        last_close = close
        local date = os.date("%Y-%m-%d", timestamps[i])
        table.insert(lines, string.format("  %s  %s", date, format_money(close)))
    end
end

if first_close and last_close then
    local pct = ((last_close - first_close) / first_close) * 100
    table.insert(lines, "")
    table.insert(lines, string.format("Period change: %.2f%% (from %s to %s)",
        pct, format_money(first_close), format_money(last_close)))
end

local stock_summary = table.concat(lines, "\n")

print("")
print("=== Market data ===")
print(stock_summary)
print("")

-- Set up the crew with two analyst agents
local crew = Crew.new({
    goal = "Bull vs Bear analyst debate on " .. TICKER,
    provider = "openai",
    model = "gpt-5.4-mini",
})

crew:add_agent(Agent.new({
    name = "bull",
    goal = "Argue the bullish case using the data; find catalysts and reasons for upside",
    capabilities = { "analysis", "argumentation" },
    system_prompt = [[
You are an optimistic equity analyst. You argue the BULL case for stocks.
Use the price data provided to build your case: momentum, valuation, position
in the 52-week range, recent trend, technical setup. Be specific and cite the
numbers. When responding to the bear, address their concerns directly and
counter with data. Keep each turn to 3-4 sentences. Do not hedge — commit
to the bullish view.
]],
}))

crew:add_agent(Agent.new({
    name = "bear",
    goal = "Argue the bearish case; find risks, headwinds, and reasons to be cautious",
    capabilities = { "analysis", "skepticism" },
    system_prompt = [[
You are a skeptical equity analyst. You argue the BEAR case for stocks.
Use the price data provided to build your case: overvaluation, exhaustion,
distance from lows, recent weakness, technical breakdowns. Be specific and
cite the numbers. When responding to the bull, address their points directly
and counter with risks. Keep each turn to 3-4 sentences. Do not hedge —
commit to the bearish view.
]],
}))

-- Create the dialog with the data snapshot baked into the starter
local debate = crew:dialog({
    agent_a = "bull",
    agent_b = "bear",
    starting_speaker = "a",
    max_turns = 4, -- 2 turns each: bull → bear → bull → bear
    starter = "Here is the recent market data for " .. TICKER .. ":\n\n" ..
              stock_summary .. "\n\n" ..
              "Based on this data alone, debate whether to BUY " .. TICKER ..
              " at the current level. Bull, make your opening case in 3-4 sentences.",
})

-- Run the debate and print the transcript
print("=== Debate ===")
print("")
local transcript = debate:run()
for _, turn in ipairs(transcript) do
    print(string.format("--- Turn %d: %s ---", turn.index + 1, string.upper(turn.agent)))
    print(turn.content)
    print("")
end

print(string.format("Debate complete: %d turns total.", #transcript))
