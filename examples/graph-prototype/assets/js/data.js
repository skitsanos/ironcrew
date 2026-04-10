/*
 * Graph content — a richer synthetic crew fixture.
 *
 * This is intentionally explicit and close to the documented surface area
 * (`agent`, `task`, `tool`, `conversation`, `dialog`, `memory`, `message`).
 * The renderer consumes these arrays and computes positions automatically,
 * so this file can later be replaced by a real Lua export.
 */

export const CREW = {
  // NOTE: `name` is not a real Crew.new() field — there is no `name` in the
  // Lua source. "research-crew" is the directory name, used here as a display
  // label for the crew node in the DAG.
  name: 'research-crew',
  provider: 'openai',
  model: 'gpt-4o-mini',
  goal: 'Produce a brief report on a topic',

  agents: [
    {
      name: 'researcher',
      goal: 'Find and analyze information on given topics',
      capabilities: ['research', 'analysis'],
      source: 'auto_discovered',  // derived: loaded from agents/researcher.lua
      temperature: 0.3,
    },
    {
      name: 'writer',
      goal: 'Write clear, well-structured content',
      capabilities: ['writing', 'summarization', 'editing'],
      tools: ['summarize'],
      source: 'auto_discovered',  // derived: loaded from agents/writer.lua
      temperature: 0.7,
    },
  ],

  tools: [
    {
      name: 'summarize',
      description: 'Summarize text to a short version',
      parameters: [{ name: 'text', type: 'string', required: true }],
      owner: 'writer',  // writer agent declares tools = {"summarize"}
      source: 'tools/summarize.lua',  // auto-loaded from the tools/ directory
    },
  ],

  memories: [],
  conversations: [],
  dialogs: [],
  functions: [],
  messages: [],

  tasks: [
    {
      id: 'research',
      name: 'Research',
      task_type: 'task',
      description: 'List 3 key benefits of using Rust for systems programming.',
      depends_on: [],
      resolved_agent: 'researcher',   // derived: auto-matched via capabilities
      assignment_source: 'auto',      // derived: no explicit agent field in crew.lua
      expected_output: 'A numbered list of 3 benefits with brief explanations',
    },
    {
      id: 'write_summary',
      name: 'Write summary',
      task_type: 'task',
      agent: 'writer',
      description: 'Write a one-paragraph summary based on these research findings:\n\n${results.research.output}',
      depends_on: ['research'],
      assignment_source: 'explicit',  // task.agent was set explicitly in crew.lua
    },
  ],
};
