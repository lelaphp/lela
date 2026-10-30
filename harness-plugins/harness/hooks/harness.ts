// The function-hook adapter of the harness (harness-design.md §4.4, §5),
// enabled by .claude/settings.json as harness@dax-php from the repository's
// own marketplace (.claude-plugin/marketplace.json).
//
// Three hooks, and nothing the shipped command hooks do not also do
// except the refresh, in-process latency, the one figure only this
// layer has and the denials kept for a desk. `tool.call` on Edit, Write and Bash
// hands the call to `xtask permit` on stdin, the shape the command hook
// reads, and returns its deny, or the tool's result with the notes as
// context; after a Bash call that ran an instrument it invalidates
// `prompt.context`, so the next read of the facts is fresh (§7, "what the
// agent sees"). `prompt.context` renders the local facts and appends them
// as one block, the `facts` of `xtask status --session --format json`,
// with the session's id so it can say "permits this session" (§4.6); the
// engine caps one block at BLOCK_CAP characters and skips a hook whose
// block is over it, so a block over the cap is cut with a note rather
// than the session losing it (#64). The map, AGENTS.md, is not a block:
// it reaches the model by the runtime's own load, CLAUDE.md's
// `@AGENTS.md`, so a session without this module still has it and lacks
// only the facts (§5, #3620 D1). `turn.complete` records what the
// session has spent (#210): `$.session.usage()` is the one live figure of
// the `/cost` ledger and the context window's fill, and it is piped once
// a turn to `xtask usage record`, which writes the reading under the
// branch's ticket, so `track work` can join it to the ticket it landed or
// left standing. Usage is a reading, not a judgement: a `usage()` that
// rejects or an `xtask` that does not run is one `$.ui.log` line and the
// turn proceeds.
//
// The command hook in hooks.json beside this file judges the same call
// beneath this one. A denial here ends the call and is recorded here, and
// the last few are kept in `$.store` as `{ rule, adr, tool, at }` for a
// desk to draw (#3489); an allow goes on to the command hook, which
// records the notes, which is what `--above-the-command-hook` tells
// permit. On Bash the hook fails open by construction (§13 item 8): a
// permit that does not run lets the call through to the command hook,
// which hands it on so a broken harness can be repaired (#16 items 2 and
// 5). An Edit or a Write no judge saw is denied here, as the command hook
// denies it, whether no candidate judged or the hook threw (by the
// `.catch`): where no managed settings load the built-in guard, another
// mod's `tool.check` can approve a call the command hook denied (#3470,
// #3489). Each candidate has the command hook's 60 s, so a cargo rebuild
// is judged rather than denied. hooks.json and
// this module are a person's files under harness.self (#2283), since it
// hands permit the evidence of the person's approval.
//
// `functionHook`: when the manifest declares it under `userConfig` and a
// person sets it false, the module registers nothing and the command
// hooks alone are exercised; undeclared, the module is on.
//
// Types: types/claude-code.d.ts beside this folder is the declarations of
// the Claude Code this checkout runs, a tracked file the tsconfig includes,
// kept current at every commit that stages the plugin and failing the
// type-check loudly when absent (#62, #1736); the import is empty at run time.

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

/// The name of the facts block. The map's block of before #3620 D1 is
/// named so a context that still carries one drops it.
const BLOCKS = { facts: 'harness-facts', map: 'harness-map' } as const;

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

/// The command hook's deny where no candidate judged an Edit or a Write,
/// word for word from hooks.json, so both layers say one thing.
const UNJUDGED =
  'xtask permit did not judge this call: every candidate refused or failed to run. An Edit or a Write no judge saw does not proceed; a Bash call still does, so the harness can be repaired. Rebuild it: cargo build -p xtask.';

/// How many denials `$.store` keeps.
const KEPT = 50;

/// The rule and first ADR a reason ends with, as `Finding::reason` writes them.
const RULE = /\(([^\s,()]+)(?:, (ADR-\d+))?[^()]*\)\s*$/;

/// One denial into `$.store` under `denials`, the last KEPT kept, for a
/// desk to draw (#3489): the rule, its ADR, the tool and when.
async function record($: EngineInterface, tool: string, reason: string): Promise<void> {
  const [, rule = null, adr = null] = RULE.exec(reason) ?? [];
  const at = new Date(await $.clock.now()).toISOString();
  const kept = await $.store.get('denials');
  await $.store.set('denials', [...(Array.isArray(kept) ? kept : []), { rule, adr, tool, at }].slice(-KEPT));
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
    // keeps permit from judging a Bash call leaves it to that hook.
    let decision: Decision = {};
    let judged = false;
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
            timeoutMs: 60_000,
          });
          if (run.exitCode === 0) {
            decision = decisionOf(run.stdout);
            judged = true;
            break;
          }
        }
        if (asked || !APPROVABLE.test(decision.deny ?? '')) break;
        const found = await $.session.messages();
        stdin = stdinOf(e, id, cwd, Array.isArray(found) ? askedOf(found) : []);
      }
    } catch (err) {
      // Bash fails open: the command hook beneath judges the call. An Edit
      // or a Write fails closed, by the `.catch` below.
      if (e.tool !== 'Bash') throw err;
    }
    if (!judged && e.tool !== 'Bash') return { deny: UNJUDGED };
    if (decision.deny !== undefined) {
      await record($, e.tool, decision.deny).catch(() => undefined);
      return { deny: decision.deny };
    }

    const result = await next(e);
    if (e.tool === 'Bash' && RAN_AN_INSTRUMENT.test(e.command)) {
      $.ui.invalidate('prompt.context');
    }
    if (decision.notes === undefined || result.deny !== undefined) return result;
    return { ...result, context: [...(result.context ?? []), decision.notes] };
  }).catch(async (_$, e, next) =>
    next.called || e.tool === 'Bash'
      ? next(e)
      : {
          deny: `harness: the function hook failed (${next.error.kind}${next.error.message ? `: ${next.error.message}` : ''}) before xtask permit judged this ${e.tool}. An Edit or a Write no judge saw does not proceed; rebuild it: cargo build -p xtask.`,
        },
  );

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
      const text = `harness: \`xtask status --session\` did not run (exit ${run.exitCode}); the local facts are missing from this session. Fix the build of xtask.\n${run.stderr}`;
      return { blocks: [...ours, capped(BLOCKS.facts, text)] };
    }
    const { facts } = JSON.parse(run.stdout) as { facts: string };
    return { blocks: [...ours, capped(BLOCKS.facts, facts)] };
  });
};
