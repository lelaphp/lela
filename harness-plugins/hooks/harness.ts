// The function-hook adapter of the harness (harness-design.md §4.4, §5).
//
// Two hooks, and nothing the shipped command hooks do not also do except
// the refresh. `prompt.context` renders the local facts and appends the
// map plus the local facts as one block, `xtask status --session`, with
// the session's id so the permits section can say "this session"
// (§4.6); it stands in for the SessionStart command hook when this plugin
// is loaded, and a person turns that hook off in hooks.json to avoid the
// map twice. `tool.call` on Bash lets the call through and, when the
// command ran an instrument, invalidates `prompt.context` so the next
// read of the facts is fresh (§7, "what the agent sees").
//
// `permit` is not called here: the PreToolUse command hook runs first on
// every Edit, Write and Bash and its deny ends the call, so a second
// judge would record every note twice in the reading log. Every hook
// fails open by construction (§13 item 8); the command hook beneath does
// not.
//
// Types: `/plugin-types` writes .claude/types/claude-code.d.ts, which the
// tsconfig beside this folder includes; the import is empty at run time.

import type { Register } from 'claude-code';

/// `cargo run` rather than a built binary: once per session, and it
/// rebuilds a stale xtask rather than trusting it (hooks.json says why).
const XTASK = ['cargo', 'run', '-q', '-p', 'xtask', '--'] as const;

/// The name of the block the map and the local facts render under.
const BLOCK = 'harness';

/// A Bash command that ran an instrument: `cargo xtask …`, `xtask …`, or
/// a built binary by path. The facts it may have changed are re-read.
const RAN_AN_INSTRUMENT = /(^|[\s;&|])(cargo\s+xtask|xtask|\S*\/xtask)\b/;

export const register: Register = (on) => {
  on('prompt.context', async ($, e, next) => {
    const out = await next(e);
    const cwd = await $.session.cwd();
    const id = await $.session.id();
    // The local file's regions are volatile: rendered by name, never by
    // discovery (§5). A computed that is missing or fails leaves the
    // last rendering in place; status says when the facts are unrendered.
    await $.process
      .run(['computed', 'run', '.claude/harness.local.md'], { cwd, timeoutMs: 60_000 })
      .catch(() => undefined);
    const run = await $.process
      .run([...XTASK, 'status', '--session', '--session-id', id], { cwd, timeoutMs: 120_000 })
      .catch((err: unknown) => ({ exitCode: -1, stdout: '', stderr: String(err) }));
    const text =
      run.exitCode === 0
        ? run.stdout
        : `harness: \`xtask status --session\` did not run (exit ${run.exitCode}); the map and the local facts are missing from this block. Fix the build of xtask.\n${run.stderr}`;
    return { blocks: [...out.blocks.filter((b) => b.name !== BLOCK), { name: BLOCK, text }] };
  });

  on('tool.call', { tool: 'Bash' }, async ($, e, next) => {
    const result = await next(e);
    if (RAN_AN_INSTRUMENT.test(e.command)) {
      $.ui.invalidate('prompt.context');
    }
    return result;
  });
};
