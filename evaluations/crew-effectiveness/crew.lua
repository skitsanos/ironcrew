-- Grounded crew-effectiveness evaluation flow.
--
-- The Python runner supplies one source-only packet and selects one of three
-- orchestration variants. The scoring oracle is never injected into this Lua
-- VM. Every variant uses the same final agent, prompt, model, temperature, and
-- JSON Schema; only the intermediate orchestration differs.

assert(input, "evaluation input is required")

local variant = assert(input.variant, "input.variant is required")
local model = assert(input.model, "input.model is required")
local case_id = assert(input.case_id, "input.case_id is required")
local packet_json = assert(input.packet_json, "input.packet_json is required")

if variant ~= "single" and variant ~= "dag" and variant ~= "collaborative" then
    error("input.variant must be single, dag, or collaborative")
end

local answer_schema = {
    type = "object",
    additionalProperties = false,
    required = { "case_id", "answers" },
    properties = {
        case_id = { type = "string" },
        answers = {
            type = "array",
            items = {
                type = "object",
                additionalProperties = false,
                required = { "question_id", "answer", "citations" },
                properties = {
                    question_id = { type = "string" },
                    answer = {
                        type = "string",
                        pattern = "^[a-z][a-z0-9_]*$",
                    },
                    citations = {
                        type = "array",
                        items = { type = "string" },
                    },
                },
            },
        },
    },
}

local crew = Crew.new({
    goal = "Evaluate grounded decision quality across IronCrew orchestration variants",
    provider = "openai",
    model = model,
    max_concurrent = 2,
})

local function add_final_agent()
    crew:add_agent(Agent.new({
        name = "integrator",
        goal = "Select the best evidence-grounded option for every evaluation question",
        system_prompt = [[
You are a rigorous evidence integrator. Every question contains explicit
single-select options. Use only the numbered evidence in the supplied packet;
never use outside knowledge. For each `answer`, copy exactly one listed option
`id`, with no label, explanation, added whitespace, or change in case. Choose
`insufficient_evidence` only when no other listed option is directly supported;
do not use it when another option accurately characterizes conflicting
evidence. Every answer must cite the evidence IDs that directly support the
selection. Return only the requested structured JSON.
]],
        temperature = 0.0,
        max_tokens = 800,
        response_format = {
            type = "json_schema",
            name = "grounded_crew_evaluation",
            schema = answer_schema,
        },
    }))
end

local final_description = table.concat({
    "IRONCREW_EVAL_STAGE:final",
    "IRONCREW_EVAL_CASE:" .. case_id,
    "Select exactly one listed option for every question. Preserve each question_id.",
    "Set answer to only that option's exact lowercase id, never its label or prose.",
    "Cite only evidence IDs that directly support the selected option.",
    "Evidence packet:",
    packet_json,
}, "\n")

if variant == "single" then
    add_final_agent()
    crew:add_task({
        name = "final",
        agent = "integrator",
        description = final_description,
        expected_output = "A JSON object matching grounded_crew_evaluation",
        max_retries = 0,
        timeout_secs = 120,
    })
elseif variant == "dag" then
    crew:add_agent(Agent.new({
        name = "extractor",
        goal = "Extract candidate option IDs and their direct evidence",
        system_prompt = "Compare the listed options using only explicit facts and calculations grounded in evidence IDs.",
        temperature = 0.0,
        max_tokens = 500,
    }))
    crew:add_agent(Agent.new({
        name = "challenger",
        goal = "Independently identify contradictions and unsupported conclusions",
        system_prompt = "Challenge candidate conclusions and flag insufficient or conflicting evidence.",
        temperature = 0.0,
        max_tokens = 500,
    }))
    add_final_agent()

    crew:add_task({
        name = "extract",
        agent = "extractor",
        description = table.concat({
            "IRONCREW_EVAL_STAGE:extract",
            "IRONCREW_EVAL_CASE:" .. case_id,
            "Compare the listed options and identify candidate option IDs with supporting evidence IDs.",
            "Evidence packet:",
            packet_json,
        }, "\n"),
        max_retries = 0,
        timeout_secs = 120,
    })
    crew:add_task({
        name = "challenge",
        agent = "challenger",
        description = table.concat({
            "IRONCREW_EVAL_STAGE:challenge",
            "IRONCREW_EVAL_CASE:" .. case_id,
            "Independently check the packet for contradictions, missing support, and calculation errors.",
            "Evidence packet:",
            packet_json,
        }, "\n"),
        max_retries = 0,
        timeout_secs = 120,
    })
    crew:add_task({
        name = "final",
        agent = "integrator",
        description = final_description,
        expected_output = "A JSON object matching grounded_crew_evaluation",
        depends_on = { "extract", "challenge" },
        max_retries = 0,
        timeout_secs = 120,
    })
else
    crew:add_agent(Agent.new({
        name = "analyst",
        goal = "Build the strongest evidence-grounded option selection for each question",
        system_prompt = "Compare the listed options using explicit evidence IDs and make a concise case.",
        temperature = 0.0,
        max_tokens = 500,
    }))
    crew:add_agent(Agent.new({
        name = "skeptic",
        goal = "Find contradictions, weak support, and unjustified certainty",
        system_prompt = "Audit the analyst's reasoning and insist on direct evidence or abstention.",
        temperature = 0.0,
        max_tokens = 500,
    }))
    add_final_agent()

    crew:add_collaborative_task({
        name = "discussion",
        description = table.concat({
            "IRONCREW_EVAL_STAGE:discussion",
            "IRONCREW_EVAL_CASE:" .. case_id,
            "Discuss the best grounded option for every question and explicitly surface uncertainty.",
            "Evidence packet:",
            packet_json,
        }, "\n"),
        agents = { "analyst", "skeptic" },
        max_turns = 1,
        max_retries = 0,
        timeout_secs = 120,
    })
    crew:add_task({
        name = "final",
        agent = "integrator",
        description = final_description,
        expected_output = "A JSON object matching grounded_crew_evaluation",
        depends_on = { "discussion" },
        max_retries = 0,
        timeout_secs = 120,
    })
end

crew:run()
