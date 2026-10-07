// The function-hook adapter of the harness (harness-design.md §4.4, §5),
// enabled by .claude/settings.json as harness@dax-php from the repository's
// own marketplace (.claude-plugin/marketplace.json).
//
// Three hooks, and nothing the shipped command hooks do not also do
// except the refresh, in-process latency and the one figure only this
// layer has. `tool.call` on Edit, Write and Bash
// hands the call to `xtask permit` on stdin, the shape the command hook
// reads, and returns its deny, or the tool's result with the notes as
// context; after a Bash call that ran an instrument it invalidates
// `prompt.context`, so the next read of the facts is fresh (§7, "what the
// agent sees"). `prompt.context` renders the local facts and appends the
// map and the local facts as two blocks, `xtask status --session --format
// json`, with the session's id so the permits section can say "this
// session" (§4.6); the engine caps one block at BLOCK_CAP characters and
// skips a hook whose block is over it, so each stands on its own and one
// over the cap is cut with a note rather than the session losing both
// (#64). The SessionStart command hook prints no map, so a session
// without this module has none (§5). `turn.complete` records what the
// session has spent (#210): `$.session.usage()` is the one live figure of
// the `/cost` ledger and the context window's fill, and it is piped once
// a turn to `xtask usage record`, which writes the reading under the
// branch's ticket, so `track work` can join it to the ticket it landed or
// left standing. Usage is a reading, not a judgement: a `usage()` that
// rejects or an `xtask` that does not run is one `$.ui.log` line and the
// turn proceeds.
//
// The command hook in hooks.json beside this file judges the same call
// beneath this one. A denial here ends the call and is recorded here; an
// allow goes on to the command hook, which records the notes, which is
// what `--above-the-command-hook` tells permit. Every hook here fails
// open by construction (§13 item 8): a permit that does not run lets the
// call through to the command hook, which on an Edit or a Write denies
// when no candidate of its own judged, and on Bash hands the call on so a
// broken harness can be repaired (#16 items 2 and 5). hooks.json is a
// person's file under harness.self; this module is not.
//
// `functionHook`: when the manifest declares it under `userConfig` and a
// person sets it false, the module registers nothing and the command
// hooks alone are exercised; undeclared, the module is on.
//
// Types: `/plugin-types` writes types/claude-code.d.ts beside this folder,
// a tracked file pre-commit rewrites when the plugin is staged (#62), which
// the tsconfig includes; the import is empty at run time.

import type { EngineInterface, Register, SessionMessage, ToolCallInput } from 'claude-code';

/// `cargo run` when no binary is built, and when no built one judged: it
/// rebuilds a stale xtask rather than trusting it (hooks.json says why),
/// at about 200 ms more per call.
const CARGO_XTASK = ['cargo', 'run', '-q', '-p', 'xtask', '--'] as const;

/// Where a built xtask stands, debug first: the command hook takes the
/// release build first, and one stale build that both adapters resolved
/// the same way took both layers out at once (#16 item 4). The debug build
/// is the one every `cargo run` and `cargo test` refreshes, so this layer
/// prefers the build that is freshest in practice while the layer beneath
/// prefers the release one, and a single stale build no longer silences
/// them together.
const BUILT = ['target/debug/xtask', 'target/release/xtask'] as const;

/// The built binaries that stand, then cargo. A binary built before the
/// manifest it reads, or before the harness's own sources moved, exits
/// without judging, so cargo closes the list rather than the last build
/// standing for it (§4.4). The order of the built two is this adapter's
/// own; the candidate that closes the list is shared, so when every build
/// is stale both adapters still end at the same rebuild.
async function xtask($: EngineInterface): Promise<readonly (readonly string[])[]> {
  const built = await Promise.all(
    BUILT.map(async (path) =>
      (await $.fs.exists(path).catch(() => false)) ? ([path] as readonly string[]) : undefined,
    ),
  );
  return [...built.filter((bin): bin is readonly string[] => bin !== undefined), CARGO_XTASK];
}

/// The names of the two blocks, the map and the local facts.
const BLOCKS = { map: 'harness-map', facts: 'harness-facts' } as const;

/// The engine's cap on one context block, in characters: a block over it
/// is "the wrong shape" and the whole hook is skipped. Stated here once;
/// `cargo test -p xtask --test map` reads this line and holds each block
/// under it with room, so a region that grows past it fails a test and
/// not a session (#64).
const BLOCK_CAP = 32_000;

/// A block held to the cap: over it, cut to fit with a line saying so,
/// since the note is worth more than the tail and the engine would
/// otherwise drop both blocks.
function capped(name: string, text: string): { name: string; text: string } {
  if (text.length <= BLOCK_CAP) return { name, text };
  const note = `\n\nharness: this block was ${text.length} characters, over the engine's cap of ${BLOCK_CAP}, and is cut here; \`cargo test -p xtask --test map\` holds it.\n`;
  return { name, text: text.slice(0, BLOCK_CAP - note.length) + note };
}

/// The tools permit judges: the command hook's matcher (§4.4).
const JUDGED = ['Edit', 'Write', 'Bash'] as const;

/// A Bash command that ran an instrument: `cargo xtask …`, `xtask …`, or
/// a built binary by path. The facts it may have changed are re-read.
const RAN_AN_INSTRUMENT = /(^|[\s;&|])(cargo\s+xtask|xtask|\S*\/xtask)\b/;

/// The hook's stdin for one call (§13 D.16): the head permit reads and
/// the tool's own arguments as `tool_input`, the event's reserved keys
/// left out; the subagent it runs in, and the answered `AskUserQuestion`
/// calls the engine holds where a person's approval is asked for (#2247).
function stdinOf(e: ToolCallInput, session_id: string, cwd: string, asked?: unknown[]): string {
  const { tool, tool_use_id, consent: _consent, agentId, ...tool_input } = e as ToolCallInput & {
    consent?: string;
  };
  return JSON.stringify({
    session_id,
    cwd,
    hook_event_name: 'PreToolUse',
    tool_name: tool,
    tool_input,
    tool_use_id,
    agent_id: agentId,
    asked,
  });
}

/// A denial the person's approval could lift, and the evidence of one:
/// each answered `AskUserQuestion` the engine recorded, never the model's text.
const APPROVABLE = /\(harness\.self(-shell)?[,)]/;
const askedOf = (messages: readonly SessionMessage[]) =>
  messages.flatMap((m) =>
    m.toolUses
      .filter((t) => t.tool === 'AskUserQuestion' && t.result !== undefined && !t.isError)
      .map((t) => ({ tool_use_id: t.tool_use_id, input: t.input, result: t.result })),
  );

/// What permit printed: the deny's reason, the allow's notes, or nothing.
type Decision = { deny?: string; notes?: string };

function decisionOf(stdout: string): Decision {
  if (stdout.trim() === '') return {};
  const head = (JSON.parse(stdout) as { hookSpecificOutput?: Record<string, string> })
    .hookSpecificOutput;
  if (head?.permissionDecision === 'deny') return { deny: head.permissionDecisionReason };
  return { notes: head?.additionalContext };
}

/// The turn `usage record` is told, counted here from the module's load:
/// a reload of the plugin starts it over, and the last reading under a
/// scope is still the session's final figure, since the ledger it carries
/// is the session's whole.
let turns = 0;

export const register: Register = (on, options) => {
  if (options.functionHook === false) return;

  on('tool.call', { tool: JUDGED }, async ($, e, next) => {
    // permit before the call, as the command hook beneath is; whatever
    // keeps permit from judging leaves the call to that hook.
    let decision: Decision = {};
    try {
      const [cwd, id, bins] = await Promise.all([$.session.cwd(), $.session.id(), xtask($)]);
      // permit's exit is 0 whatever it decides, so a non-zero one is a
      // permit that never judged: the build is older than the manifest it
      // reads, or than the harness's own sources. The next candidate
      // judges, rather than the call passing on a build's silence. A
      // denial an approval could lift is asked again with the evidence.
      let stdin = stdinOf(e, id, cwd);
      for (let asked = false; ; asked = true) {
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
        if (asked || !APPROVABLE.test(decision.deny ?? '')) break;
        const found = await $.session.messages();
        stdin = stdinOf(e, id, cwd, Array.isArray(found) ? askedOf(found) : []);
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

  on('turn.complete', async ($, e, next) => {
    const out = await next(e);
    turns += 1;
    const turn = turns;
    try {
      const [cwd, id, bins, usage] = await Promise.all([
        $.session.cwd(),
        $.session.id(),
        xtask($),
        $.session.usage(),
      ]);
      const stdin = JSON.stringify(usage);
      // The candidates as permit walks them, the built binaries and then
      // cargo; a build that does not know `usage` exits without writing,
      // and the next one records the turn.
      let recorded = false;
      for (const bin of bins) {
        const run = await $.process.run(
          [...bin, 'usage', 'record', '--session', id, '--turn', String(turn)],
          { cwd, stdin, timeoutMs: 120_000 },
        );
        if (run.exitCode === 0) {
          recorded = true;
          break;
        }
      }
      if (!recorded) {
        $.ui.log(`harness: turn ${turn}'s usage was not recorded; no xtask ran \`usage record\``);
      }
    } catch (err) {
      $.ui.log(`harness: turn ${turn}'s usage was not recorded: ${String(err)}`);
    }
    return out;
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
      .run([...CARGO_XTASK, 'status', '--session', '--session-id', id, '--format', 'json'], {
        cwd,
        timeoutMs: 120_000,
      })
      .catch((err: unknown) => ({ exitCode: -1, stdout: '', stderr: String(err) }));
    const ours = out.blocks.filter((b) => b.name !== BLOCKS.map && b.name !== BLOCKS.facts);
    if (run.exitCode !== 0) {
      const text = `harness: \`xtask status --session\` did not run (exit ${run.exitCode}); the map and the local facts are missing from this session. Fix the build of xtask.\n${run.stderr}`;
      return { blocks: [...ours, capped(BLOCKS.map, text)] };
    }
    const { map, facts } = JSON.parse(run.stdout) as { map: string; facts: string };
    return { blocks: [...ours, capped(BLOCKS.map, map), capped(BLOCKS.facts, facts)] };
  });
};
