// The function-hook adapter of the harness (harness-design.md §4.4, §5),
// enabled by .claude/settings.json as harness@dax-php from the repository's
// own marketplace (.claude-plugin/marketplace.json).
//
// Two hooks, and nothing the shipped command hooks do not also do except
// the refresh and in-process latency. `tool.call` on Edit, Write and Bash
// hands the call to `xtask permit` on stdin, the shape the command hook
// reads, and returns its deny, or the tool's result with the notes as
// context; after a Bash call that ran an instrument it invalidates
// `prompt.context`, so the next read of the facts is fresh (§7, "what the
// agent sees"). `prompt.context` renders the local facts and appends the
// map plus the local facts as one block, `xtask status --session`, with
// the session's id so the permits section can say "this session" (§4.6);
// the SessionStart command hook prints no map, so a session without this
// module has none (§5).
//
// The command hook in hooks.json beside this file judges the same call
// beneath this one. A denial here ends the call and is recorded here; an
// allow goes on to the command hook, which records the notes, which is
// what `--above-the-command-hook` tells permit. Every hook here fails
// open by construction (§13 item 8): a permit that does not run lets the
// call through to the command hook, which does not fail open. hooks.json
// is a person's file under harness.self; this module is not.
//
// `functionHook`: when the manifest declares it under `userConfig` and a
// person sets it false, the module registers nothing and the command
// hooks alone are exercised; undeclared, the module is on.
//
// Types: `/plugin-types` writes .claude/types/claude-code.d.ts, which the
// tsconfig beside this folder includes; the import is empty at run time.

import type { EngineInterface, Register, ToolCallInput } from 'claude-code';

/// `cargo run` when no binary is built, and when no built one judged: it
/// rebuilds a stale xtask rather than trusting it (hooks.json says why),
/// at about 200 ms more per call.
const CARGO_XTASK = ['cargo', 'run', '-q', '-p', 'xtask', '--'] as const;

/// Where a built xtask stands, in the command hook's order (hooks.json).
const BUILT = ['target/release/xtask', 'target/debug/xtask'] as const;

/// The built binaries that stand, then cargo: the command hook's choice,
/// so the two adapters cost the same at the tool call (§4.4). A binary
/// built before the manifest it reads exits without judging, so cargo
/// closes the list rather than the last build standing for it.
async function xtask($: EngineInterface): Promise<readonly (readonly string[])[]> {
  const built = await Promise.all(
    BUILT.map(async (path) =>
      (await $.fs.exists(path).catch(() => false)) ? ([path] as readonly string[]) : undefined,
    ),
  );
  return [...built.filter((bin): bin is readonly string[] => bin !== undefined), CARGO_XTASK];
}

/// The name of the block the map and the local facts render under.
const BLOCK = 'harness';

/// The tools permit judges: the command hook's matcher (§4.4).
const JUDGED = ['Edit', 'Write', 'Bash'] as const;

/// A Bash command that ran an instrument: `cargo xtask …`, `xtask …`, or
/// a built binary by path. The facts it may have changed are re-read.
const RAN_AN_INSTRUMENT = /(^|[\s;&|])(cargo\s+xtask|xtask|\S*\/xtask)\b/;

/// The hook's stdin for one call (§13 D.16): the head permit reads and
/// the tool's own arguments as `tool_input`, the event's reserved keys
/// left out.
function stdinOf(e: ToolCallInput, session_id: string, cwd: string): string {
  const { tool, tool_use_id, consent: _consent, ...tool_input } = e as ToolCallInput & {
    consent?: string;
  };
  return JSON.stringify({
    session_id,
    cwd,
    hook_event_name: 'PreToolUse',
    tool_name: tool,
    tool_input,
    tool_use_id,
  });
}

/// What permit printed: the deny's reason, the allow's notes, or nothing.
type Decision = { deny?: string; notes?: string };

function decisionOf(stdout: string): Decision {
  if (stdout.trim() === '') return {};
  const head = (JSON.parse(stdout) as { hookSpecificOutput?: Record<string, string> })
    .hookSpecificOutput;
  if (head?.permissionDecision === 'deny') return { deny: head.permissionDecisionReason };
  return { notes: head?.additionalContext };
}

export const register: Register = (on, options) => {
  if (options.functionHook === false) return;

  on('tool.call', { tool: JUDGED }, async ($, e, next) => {
    // permit before the call, as the command hook beneath is; whatever
    // keeps permit from judging leaves the call to that hook.
    let decision: Decision = {};
    try {
      const [cwd, id, bins] = await Promise.all([$.session.cwd(), $.session.id(), xtask($)]);
      const stdin = stdinOf(e, id, cwd);
      // permit's exit is 0 whatever it decides, so a non-zero one is a
      // permit that never judged: the build is older than the manifest it
      // reads. The next candidate judges, rather than the call passing on
      // a build's silence.
      for (const bin of bins) {
        const run = await $.process.run([...bin, 'permit', '--above-the-command-hook'], {
          cwd,
          stdin,
          timeoutMs: 30_000,
        });
        if (run.exitCode === 0) {
          decision = decisionOf(run.stdout);
          break;
        }
      }
    } catch {
      // fails open: the command hook beneath judges the call
    }
    if (decision.deny !== undefined) return { deny: decision.deny };

    const result = await next(e);
    if (e.tool === 'Bash' && RAN_AN_INSTRUMENT.test(e.command)) {
      $.ui.invalidate('prompt.context');
    }
    if (decision.notes === undefined || result.deny !== undefined) return result;
    return { ...result, context: [...(result.context ?? []), decision.notes] };
  });

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
      .run([...CARGO_XTASK, 'status', '--session', '--session-id', id], { cwd, timeoutMs: 120_000 })
      .catch((err: unknown) => ({ exitCode: -1, stdout: '', stderr: String(err) }));
    const text =
      run.exitCode === 0
        ? run.stdout
        : `harness: \`xtask status --session\` did not run (exit ${run.exitCode}); the map and the local facts are missing from this block. Fix the build of xtask.\n${run.stderr}`;
    return { blocks: [...out.blocks.filter((b) => b.name !== BLOCK), { name: BLOCK, text }] };
  });
};
