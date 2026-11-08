---
name: retro
description: "Conduct a retrospective on a coding session."
disable-model-invocation: true
---

The user has asked for a **retrospective**. You are suggesting improvements to the coding agent's **environment** to improve future runs.

## Steps

1. Call the Skill tool with `writing-for-agents` for the writing style guide.

2. Read the primary sources for the session the user specifies: its transcript under `~/.claude/projects/-Users-somebody-Projects-dax-php/`, read only, and what the harness recorded with `cargo xtask status`. If the user doesn't specify a session, default to the current one.

3. Look for candidates for improvement in these categories.

- **Navigation**: how easy was it for the agent to find the right files? Are there hidden dependencies between files? Would a **navigation pointer** make it easier? _Use when_ the session took a long time to find a piece of information.
- **Automated checks**: are there automated checks that could catch errors the agent made? Linting, typing, tests, filesystem linters? Read `cargo xtask --help` and `cargo xtask status --section rules` first, so a check that already exists but sits unwired or silently broken is the finding, not a reinvention. _Use when_ the agent made a mistake an automated check could have caught.
- **Coding standards**: should the **reviewer agent** be given a new rule to enforce? Should an existing rule be removed or clarified? Classify the violation first: a **mechanical** one (a fixed syntactic pattern, a banned API, an import shape, a file-location rule) gets a deterministic check, full stop: a rule proposed with the fixture that would fail, since `seams.toml` stays a person's. Default to the check over a sentence. A **judgement call** (cross-file consistency, "matches the surrounding style," anything no check could ever substitute for) goes to `code-review`'s Standards axis. A guide you propose is written as a literal command to copy, since those hold without a sensor. _Use when_ the reviewer agent failed to catch a mistake.
- **AGENTS.md**: are there any steering instructions that should move to a skill or an automated check instead? AGENTS.md prose is a person's to word; a skill's text is an agent's. _Use when_ a sentence there is ignored or contradicted, in the repo or the user's global scope.
- **Tool economy**: did the agent make expensive tool calls that could be streamlined? Is there any custom tooling (CLI's, MCP's) that is particularly token-inefficient? _Use when_ the agent made an expensive tool call.
- **No-ops**: look for instructions in steering files that don't modify the agent's behavior. _Use when_ the steering files are large and unwieldy.
- **Information access**: look for opportunities to increase the agent's access to information. Teeing dev server logs, readonly access to third-party services. _Use when_ a crucial piece of information was not available to the agent.

4. Present these candidates to the user, in order of severity. Once the user picks, search open tickets for each kept candidate, then file it: `for-a-person` where it touches a harness file, otherwise `ready-for-agent`. A candidate kept only in the conversation dies with it.

## Reference

### Implementation vs Review

Remember that all work goes through two stages: implementation and review. The implementation agent has the most **context pressure**. They are responsible for exploration, writing code, and debugging failures.

The review agent has the least context pressure - it receives a diff, so no exploration needed. It often does not need to write code or debug.

This means that the review agent should be responsible for imposing coding standards, not the implementation agent.

### Files

You have access to several files in the repo:

- `CLAUDE.md`/`AGENTS.md`: these files are pushed to the context window of any agent working in this repo. They should be used incredibly sparingly, usually only for **navigation pointers** to other files.
- `CONTEXT.md`: the glossary. Its `_Avoid_` lists are held by `xtask vocab`.
- `seams.toml`: the manifest every rule sensor derives from. A person's file.
- Docs: use docs as references files, pointed to by other files. Look for existing docs before writing new ones.
- Skills: use skills for docs (since their description goes into the agent's context window), or for user-invoked commands. Follow the advice in the `writing-for-agents` skill.
