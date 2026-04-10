--[[
    Vision Example

    Demonstrates image input support with crew:conversation().
    Uses Gemini Flash via the OpenAI-compatible endpoint by default.

    Setup for Gemini:
        export OPENAI_API_KEY=your-gemini-api-key
        export OPENAI_MODEL=gemini-2.5-flash
        export OPENAI_BASE_URL=https://generativelanguage.googleapis.com/v1beta/openai

    Or for GPT-4o:
        export OPENAI_API_KEY=your-openai-key
        export OPENAI_MODEL=gpt-4o

    Image: uses ironcrew-cover.jpg from the repo root
]]

local crew = Crew.new({
    goal = "Analyze an image",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gemini-2.5-flash",
    base_url = env("OPENAI_BASE_URL")
        or "https://generativelanguage.googleapis.com/v1beta/openai",
})

local conv = crew:conversation({ agent = "analyst" })

print("Sending image to vision model...")
print()

-- Image path is relative to the project directory (examples/vision/)
local reply = conv:send(
    "Describe what you see in this image. Be specific about colors, objects, text, and composition.",
    { images = { "../../ironcrew-cover.jpg" } }
)

print("=== Vision Analysis ===")
print()
print(reply)
