--[[
    Vision Example

    Demonstrates image input support with crew:conversation().
    Uses GPT-5.6 Luna via OpenAI by default.

    Setup for OpenAI:
        export OPENAI_API_KEY=your-openai-key
        export OPENAI_MODEL=gpt-5.6-luna
        export OPENAI_BASE_URL=https://api.openai.com/v1

    Or for Gemini:
        export OPENAI_API_KEY=your-gemini-api-key
        export OPENAI_MODEL=gemini-2.5-flash
        export OPENAI_BASE_URL=https://generativelanguage.googleapis.com/v1beta/openai

    Image: defaults to the public IronCrew cover image. Pass input.image to
    use another public URL or a project-relative image file.
]]

local image = (input and input.image)
    or "https://raw.githubusercontent.com/skitsanos/ironcrew/develop/ironcrew-cover.jpg"

local crew = Crew.new({
    goal = "Analyze an image",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-5.6-luna",
    base_url = env("OPENAI_BASE_URL"),
})

local conv = crew:conversation({ agent = "analyst" })

print("Sending image to vision model...")
print()

local reply = conv:send(
    "Describe what you see in this image. Be specific about colors, objects, text, and composition.",
    { images = { image } }
)

print("=== Vision Analysis ===")
print()
print(reply)
