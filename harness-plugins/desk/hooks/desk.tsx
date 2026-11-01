// The desk, stage one (#3496, ruled 2026-10-25 through a grilling of
// research/harness-mods.r1.md §5; harness-design.md §6): the decisions a
// person owes, beside the session that waits on them, enabled by
// .claude/settings.json as desk@dax-php from the repository's own
// marketplace. A ruling here admits nothing a ticket comment does not
// admit today: a harness-file change still lands by `xtask steer` or
// #2247's question, and the desk never writes a file. A press is a
// person's approval, never a person's hand.
//
// Four parts, one module:
//
// - The queue. From `session.start` a timer runs `gh issue list --label
//   for-a-person` and `xtask ticket read <n>` for each ticket changed since
//   the last sweep, keeps the rows in `$.store`, and says
//   `harness: N decisions waiting` with `$.ui.status`, stale where the
//   latest sweep failed. It opens no pane.
// - The pane. `/decide` opens it: per ticket what it asks, what it blocks,
//   where the ruling lands, and Discuss, Rule and Not now.
// - The counsel, of the type `agent.register` answers (the engine names it
//   `<plugin>:<name>`, so `desk:counsel`; the ruling spelled it
//   `harness:counsel`): read-only, it states the case for each option and
//   drafts, and never rules. The model is never offered it, its
//   SendMessage to one is refused, and its Bash is held to the reads
//   below, with `permit` still judging beneath.
// - The ruling, posted from the Input's submit; no stronger than a comment
//   posted with the person's token (#3746). `Ruled <date> (<person>, desk,
//   session <id>): …` carries the option picked, by its label, and the
//   words the person typed, which a counsel's later draft never replaces;
//   the counsel's draft posted unedited says so, and a ticket changed since
//   the pick posts nothing (#3831). A notice joins the main conversation,
//   the ticket's "A person rules" boxes are ticked (#3785), a ruling that
//   lands no harness file runs `xtask ticket close` with what the person
//   enters it opened, and a session waiting on exactly the ticket it
//   unblocks is told so in a row its model reads.
//
// The desk counts in `$.store` each ruling a later #2247 question names,
// the number stage two's ticket cites.

import type { EngineInterface, Register, ToolCallInput } from 'claude-code';

/// The pane's id and the command that opens it.
const PANE = 'decide';
/// The counsel's name; its type is what `agent.register` answers.
const COUNSEL = 'counsel';
/// How often the queue is swept: a sweep is one `gh` call and a
/// `ticket read` per ticket changed since the last.
const EVERY_MS = 5 * 60_000;
/// Turns of one counsel after which its row says so; Discuss continues.
const SOFT_CAP = 20;
/// The candidates `harness.ts` walks: a built xtask, then cargo.
const BUILT = ['target/debug/xtask', 'target/release/xtask'] as const;
const CARGO_XTASK = ['cargo', 'run', '-q', '-p', 'xtask', '--'] as const;

/// One for-a-person ticket as the pane draws it.
type Row = {
  n: number;
  title: string;
  /// The "What to decide" section, or the ticket's opening where it has none.
  asks: string;
  /// The options the ticket names, as bullets under its options or proposal.
  options: string[];
  /// The open tickets it blocks.
  blocks: number[];
  /// The harness files its body names; none is a ruling that closes it.
  lands: string[];
  /// `xtask ticket read`'s line, of the ticket as last changed.
  criteria: string;
  updatedAt: string;
};

/// The ticket in Rule: the `updatedAt` its options were drawn at, the
/// counsel's draft as it stood when Rule was pressed, what the person has
/// typed, and the option picked, by its label.
type Pick = { n: number; at: string; draft: string; typed?: string; option?: string };
/// One counsel of a session, by its agent id.
type Counsel = { n: number; session: string; turns: number; draft?: string };
/// One ruling posted, and whether a later #2247 question named its ticket.
type Ruling = { n: number; at: string; session: string; confirmed: boolean };

/// What `gh issue list --json` answers for one ticket.
type Listed = {
  number: number;
  title: string;
  body: string;
  updatedAt: string;
  blocking?: { nodes?: { number: number; state: string }[] };
};

/// The paths a ruling cannot land by itself: harness.self's scope.
const HARNESS_FILE =
  /(?:seams\.toml|oracle\.toml|docs\/divergence\.toml|\.claude-plugin\/marketplace\.json|harness-plugins\/[\w.-]+\/(?:hooks|\.claude-plugin)\/[\w./-]*|\.claude\/(?!skills\/)[\w./-]+)/g;

/// The body under one `## ` heading, up to the next.
function section(body: string, heading: RegExp): string | undefined {
  const lines = body.split('\n');
  const at = lines.findIndex((l) => /^##\s/.test(l) && heading.test(l));
  if (at < 0) return undefined;
  const end = lines.findIndex((l, i) => i > at && /^##\s/.test(l));
  const text = lines.slice(at + 1, end < 0 ? undefined : end).join('\n').trim();
  return text === '' ? undefined : text;
}

/// The bullets of a text, each cut to a Select's line.
function bullets(text: string): string[] {
  return text
    .split('\n')
    .map((l) => /^\s*(?:[-*]|\d+[.)])\s+(.+)$/.exec(l)?.[1])
    .filter((l): l is string => l !== undefined)
    .map((l) => l.replace(/\*\*/g, '').trim())
    .map((l) => (l.length > 120 ? `${l.slice(0, 119)}…` : l));
}

/// One ticket as a row, its criteria line kept from the sweep that read it.
function rowOf(issue: Listed, criteria: string): Row {
  const body = issue.body ?? '';
  const asks =
    section(body, /what to decide/i) ??
    section(body, /what to build|what happened/i) ??
    body.split('\n\n')[0]?.trim() ??
    '';
  const offered = section(body, /option|proposal/i);
  return {
    n: issue.number,
    title: issue.title,
    asks: asks.length > 600 ? `${asks.slice(0, 599)}…` : asks,
    options: bullets(offered ?? asks),
    blocks: (issue.blocking?.nodes ?? []).filter((b) => b.state === 'OPEN').map((b) => b.number),
    lands: [...new Set(body.match(HARNESS_FILE) ?? [])],
    criteria,
    updatedAt: issue.updatedAt,
  };
}

/// #3785's ruling: the boxes under `## Acceptance criteria` that read
/// "A person rules…" ticked, their text unchanged, and how many.
function ticked(body: string): { body: string; count: number } {
  let criteria = false;
  let count = 0;
  const lines = body.split('\n').map((line) => {
    if (/^##\s/.test(line)) criteria = /^##\s+acceptance criteria/i.test(line);
    const tick = criteria ? line.replace(/^(\s*[-*]\s+)\[ \](\s+A person rules\b)/, '$1[x]$2') : line;
    count += tick === line ? 0 : 1;
    return tick;
  });
  return { body: lines.join('\n'), count };
}

/// A JSON text read, or undefined where it is none.
function parsed<T>(text: string): T | undefined {
  try {
    return JSON.parse(text) as T;
  } catch {
    return undefined;
  }
}

const firstLine = (text: string) => text.trim().split('\n')[0] ?? '';

/// The built xtask binaries that stand, then cargo, as harness.ts walks them.
async function xtask($: EngineInterface): Promise<(readonly string[])[]> {
  const built = await Promise.all(BUILT.map(async (p) => ((await $.fs.exists(p).catch(() => false)) ? p : '')));
  return [...built.filter((p) => p !== '').map((p) => [p]), CARGO_XTASK];
}

/// The first line an xtask candidate printed that starts with one of
/// `heads`, walking the candidates until one does.
async function xtaskLine($: EngineInterface, args: string[], heads: RegExp): Promise<string | undefined> {
  const cwd = await $.session.cwd();
  for (const bin of await xtask($)) {
    const run = await $.process.run([...bin, ...args], { cwd, timeoutMs: 120_000 }).catch(() => undefined);
    const line = `${run?.stdout ?? ''}\n${run?.stderr ?? ''}`.split('\n').find((l) => heads.test(l));
    if (line !== undefined) return line.trim();
  }
  return undefined;
}

/// The JSON the first xtask candidate that exits 0 prints.
async function xtaskJson<T>($: EngineInterface, args: string[]): Promise<T | undefined> {
  const cwd = await $.session.cwd();
  for (const bin of await xtask($)) {
    const run = await $.process.run([...bin, ...args], { cwd, timeoutMs: 120_000 }).catch(() => undefined);
    const value = run?.exitCode === 0 ? parsed<T>(run.stdout) : undefined;
    if (value !== undefined) return value;
  }
  return undefined;
}

/// A ticket as it stands at the tracker, or why it could not be read.
async function standing($: EngineInterface, n: number, cwd: string): Promise<{ updatedAt: string; body: string } | string> {
  const argv = ['gh', 'issue', 'view', String(n), '--json', 'updatedAt,body'];
  const run = await $.process.run(argv, { cwd, timeoutMs: 60_000 }).catch((err: unknown) => String(err));
  if (typeof run === 'string') return `gh issue view did not answer: ${run}`;
  if (run.exitCode !== 0) return `gh issue view exited ${run.exitCode}: ${firstLine(run.stderr)}`;
  const seen = parsed<{ updatedAt?: unknown; body?: unknown }>(run.stdout);
  return typeof seen?.updatedAt === 'string' && typeof seen.body === 'string'
    ? { updatedAt: seen.updatedAt, body: seen.body }
    : 'gh issue view answered no ticket';
}

const rowsOf = (v: unknown): Row[] => (Array.isArray(v) ? (v as Row[]) : []);
const counselsOf = (v: unknown): Record<string, Counsel> =>
  v !== null && typeof v === 'object' ? (v as Record<string, Counsel>) : {};
const rulingsOf = (v: unknown): Ruling[] => (Array.isArray(v) ? (v as Ruling[]) : []);
const text = (t: string) => [{ type: 'text' as const, text: t }];
const clockOf = (ms: number) => `${new Date(ms).toISOString().slice(11, 16)} UTC`;

/// The counsel's type as `agent.register` answered it, and the counsel ids
/// of this process, read back from `$.store` at each `session.start`, so
/// the tool-call guards judge without a store read.
let counselType: string | undefined;
const counselIds = new Set<string>();
/// Tickets set aside with Not now, the one in Rule, what the last post and
/// close of a ticket came to, the tickets ruled, the posts under way, and
/// when the queue was last swept and how the latest sweep failed: this
/// session's alone.
const later = new Set<number>();
let ruling: Pick | undefined;
const outcome = new Map<number, string>();
const closing = new Map<number, string>();
const ruled = new Set<number>();
const posting = new Set<number>();
let sweeping = false;
let swept: { at?: string; failed?: string } = {};

/// One sweep: the list, a `ticket read` for each ticket changed since the
/// last, the rows into `$.store`; undefined, or why the queue was not swept.
async function sweepOnce($: EngineInterface): Promise<string | undefined> {
  const cwd = await $.session.cwd();
  const argv = ['gh', 'issue', 'list', '--label', 'for-a-person', '--state', 'open', '--limit', '200', '--json', 'number,title,body,updatedAt,blocking'];
  const run = await $.process.run(argv, { cwd, timeoutMs: 60_000 }).catch((err: unknown) => String(err));
  if (typeof run === 'string') return `gh issue list did not answer: ${run}`;
  if (run.exitCode !== 0) return `gh exited ${run.exitCode}: ${firstLine(run.stderr)}`;
  const listed = parsed<Listed[]>(run.stdout);
  if (!Array.isArray(listed)) return 'gh issue list answered no JSON';
  const before = new Map(rowsOf(await $.store.get('queue')).map((r) => [r.n, r]));
  const rows: Row[] = [];
  for (const issue of listed) {
    const kept = before.get(issue.number);
    const criteria =
      kept !== undefined && kept.updatedAt === issue.updatedAt
        ? kept.criteria
        : ((await xtaskLine($, ['ticket', 'read', String(issue.number)], /^(pass|fail|skip) /)) ?? 'ticket read did not run');
    rows.push(rowOf(issue, criteria));
  }
  await $.store.set('queue', rows);
  return undefined;
}

/// A sweep, and the count into the status line: stale since the last sweep
/// that ran, where this one failed.
async function sweep($: EngineInterface): Promise<void> {
  if (sweeping) return;
  sweeping = true;
  try {
    const at = clockOf(await $.clock.now());
    const failed = await sweepOnce($).catch((err: unknown) => String(err));
    if (failed !== undefined) $.ui.log(`desk: the queue was not swept: ${failed}`);
    swept = failed === undefined ? { at } : { at: swept.at, failed: `the sweep at ${at} failed: ${failed}` };
    const rows = rowsOf(await $.store.get('queue')).length;
    const stale = failed === undefined ? '' : `, stale since ${swept.at ?? 'an earlier session'}`;
    $.ui.status(rows === 0 && stale === '' ? undefined : `harness: ${rows} decision${rows === 1 ? '' : 's'} waiting${stale}`);
    $.ui.invalidate('ui.render');
  } finally {
    sweeping = false;
  }
}

/// The counsel's system prompt: the case for each option, cited, and a
/// draft the person edits; never a ruling.
const COUNSEL_PROMPT = `You are the counsel on one for-a-person ticket of the dax-php repository. A person rules on it; you never do.

Read the ticket first with \`gh issue view <n> --comments\`, then what it cites: the design documents (third-engine-design.md, harness-design.md, jit-design.md), the ADRs under docs/adr/, the manifest seams.toml, and php-src at the tag oracle.toml pins where the question touches php's behaviour.

Then, for each option the ticket names, state the case for it with the passage that supports it, cited as \`harness-design.md §11\` or \`ADR-0006\`, what it would change, and what it leaves a person to do. Where a source decides the question, say which passage does. Answer what the person asks in the same way.

You do not rule, write a file, post to the tracker or close a ticket. Your Bash runs \`cargo xtask status\` and \`cargo xtask ticket read\` (or the same through \`target/debug/xtask\` or \`target/release/xtask\`), \`git log\`, \`git show\`, \`git diff\` and \`gh issue view\` alone, one command per call, with no pipe or redirection.

End every answer with one line that begins \`Draft ruling:\` and gives a ruling in plain words for the person to edit or discard. It is a draft, not a ruling.`;

/// The counsel's Bash: the reads the ruling names, one command, no shell;
/// xtask as cargo runs it or as the checkout built it, never one by any
/// other path, which is whatever stands there.
const COUNSEL_BASH =
  /^(?:(?:cargo\s+xtask|(?:\.\/)?target\/(?:debug|release)\/xtask)\s+(?:status|ticket\s+read)|git\s+(?:log|show|diff)|gh\s+issue\s+view)(?:\s|$)/;
const SHELL = /[;&|`$<>(){}\\\n]|--output/;
const READS = ['Read', 'Grep', 'Glob'];

/// Why a counsel's call is refused, or undefined where it may go on to
/// the plugins beneath, `permit` among them.
function counselRefuses(e: ToolCallInput): string | undefined {
  if (READS.includes(String(e.tool))) return undefined;
  if (e.tool === 'Bash') {
    const command = e.command.trim();
    return COUNSEL_BASH.test(command) && !SHELL.test(command)
      ? undefined
      : "desk: the counsel's Bash runs cargo xtask status, cargo xtask ticket read, git log, git show, git diff and gh issue view alone, one command with no pipe or redirection";
  }
  return `desk: the counsel reads and drafts; it calls no ${String(e.tool)}`;
}

/// Whether a SendMessage recipient is a counsel: its agent id, or the
/// name the engine gave it, a listing's ` [ref]` dropped.
async function isCounsel($: EngineInterface, to: string): Promise<boolean> {
  const bare = to.replace(/\s*\[[^\]]*\]\s*$/, '').trim();
  if (counselIds.has(bare)) return true;
  const listed = await $.agent.list();
  return listed.some((a) => a.type === counselType && (a.id === bare || a.name === bare));
}

/// The brief a counsel starts from.
const briefOf = (row: Row) =>
  `Counsel the person on #${row.n}, "${row.title}". Read it with \`gh issue view ${row.n} --comments\` first, then lay out the case for each of its options.`;

/// Discuss: a counsel for the row's ticket, this session's standing one or
/// a new one, and the view moved to it by the person's own press.
async function discuss($: EngineInterface, row: Row): Promise<void> {
  const session = await $.session.id();
  const counsels = counselsOf(await $.store.get('counsels'));
  const live = new Set((await $.agent.list()).map((a) => a.id));
  let agentId = Object.entries(counsels).find(([id, c]) => c.n === row.n && c.session === session && live.has(id))?.[0];
  if (agentId === undefined) {
    const type = counselType;
    if (type === undefined) return void $.ui.toast(`desk: no counsel for #${row.n}: its type was not registered`);
    const spawned = await $.agent.spawn({ subagentType: type, description: `#${row.n}`, prompt: briefOf(row) });
    // The id the spawn answers, or else the new counsel the agent list shows.
    agentId =
      spawned.agentId ??
      (await $.agent.list()).find((a) => a.type === type && a.description === `#${row.n}` && !(a.id in counsels))?.id;
    if (agentId === undefined) {
      $.ui.toast(`desk: no counsel for #${row.n}: ${spawned.deny ?? 'it did not start'}`);
      return;
    }
    counselIds.add(agentId);
    await $.store.set('counsels', { ...counsels, [agentId]: { n: row.n, session, turns: 0 } });
  }
  const shown = await $.ui.view({ agentId });
  if (shown.deny !== undefined) $.ui.toast(`desk: the view stayed: ${shown.deny}`);
  $.ui.invalidate('ui.render');
}

/// The ruling: posted only from the Input's submit, the person's words
/// verbatim after the header, with the option they picked in the Select.
async function rule($: EngineInterface, row: Row, words: string): Promise<void> {
  const picked = ruling?.n === row.n ? ruling : undefined;
  if (picked === undefined || posting.has(row.n) || ruled.has(row.n)) return;
  // The words submitted stay in the Input, whatever the post comes to.
  picked.typed = words;
  if (words.trim() === '' && picked.option === undefined) {
    $.ui.toast(`desk: #${row.n}: nothing to post; type the ruling or pick an option`);
    return;
  }
  posting.add(row.n);
  try {
    await post($, row, words, picked);
  } catch (err) {
    outcome.set(row.n, `not posted: ${String(err)}`);
  } finally {
    posting.delete(row.n);
    $.ui.invalidate('ui.render');
  }
}

/// The post itself, one at a time per ticket, and only while the ticket
/// stands as it did at the pick: a comment that timed out may have landed,
/// and moved `updatedAt`, so a second submit never posts it twice.
async function post($: EngineInterface, row: Row, words: string, picked: Pick): Promise<void> {
  const [cwd, session, now] = await Promise.all([$.session.cwd(), $.session.id(), $.clock.now()]);
  const seen = await standing($, row.n, cwd);
  if (typeof seen === 'string') return void outcome.set(row.n, `not posted: ${seen}`);
  if (seen.updatedAt !== picked.at) {
    const moved = `${picked.at}, now ${seen.updatedAt}`;
    return void outcome.set(row.n, `not posted: #${row.n} changed at the tracker since the pick (${moved}); read it and pick again`);
  }
  const at = new Date(now).toISOString();
  const name = await $.process.run(['git', 'config', 'user.name'], { cwd }).catch(() => undefined);
  const person = name?.stdout.trim().split(/\s+/)[0] || 'a person';
  const unedited = picked.draft.trim() !== '' && words.trim() === picked.draft.trim();
  const head = `Ruled ${at.slice(0, 10)} (${person}, desk, session ${session.slice(0, 8)}${unedited ? "; the counsel's draft, unedited" : ''}):`;
  const said = [picked.option, words].filter((s): s is string => s !== undefined && s.trim() !== '').join('\n\n');
  const comment = ['gh', 'issue', 'comment', String(row.n), '--body-file', '-'];
  const posted = await $.process.run(comment, { cwd, stdin: `${head} ${said}`, timeoutMs: 60_000 }).catch((err: unknown) => String(err));
  if (typeof posted === 'string') {
    const again = `a second submit posts only while #${row.n} stands as it did at the pick`;
    return void outcome.set(row.n, `not known whether the ruling posted: ${posted}; ${again}`);
  }
  if (posted.exitCode !== 0) return void outcome.set(row.n, `not posted: gh exited ${posted.exitCode}: ${firstLine(posted.stderr)}`);
  ruling = undefined;
  ruled.add(row.n);
  await $.store.set('rulings', [...rulingsOf(await $.store.get('rulings')), { n: row.n, at, session, confirmed: false }]);
  await $.session.append({ message: { type: 'system', content: text(`desk: #${row.n} ruled by ${person} and posted to the ticket`) } });
  const lands = row.lands.length === 0 ? '' : `; it lands in ${row.lands.join(', ')}, by steer or #2247's question`;
  outcome.set(row.n, `ruled and posted; ${await tick($, row.n, cwd, seen.body)}${lands}`);
  await unblock($, row, cwd);
  $.ui.toast(`desk: #${row.n} ruled`);
}

/// The "A person rules" boxes ticked (#3785's ruling), in the body as it
/// stood at the check before the post, and only while it still does.
async function tick($: EngineInterface, n: number, cwd: string, checked: string): Promise<string> {
  const next = ticked(checked);
  if (next.count === 0) return 'no "A person rules" box to tick';
  const now = await standing($, n, cwd);
  const changed = typeof now === 'string' ? now : now.body === checked ? undefined : 'the body changed after the check';
  if (changed !== undefined) return `the "A person rules" box is not ticked: ${changed}`;
  const edit = ['gh', 'issue', 'edit', String(n), '--body-file', '-'];
  const run = await $.process.run(edit, { cwd, stdin: next.body, timeoutMs: 60_000 }).catch((err: unknown) => String(err));
  if (typeof run === 'string' || run.exitCode !== 0) {
    return `the "A person rules" box is not ticked: ${typeof run === 'string' ? run : `gh exited ${run.exitCode}: ${firstLine(run.stderr)}`}`;
  }
  return `ticked ${next.count} "A person rules" box${next.count === 1 ? '' : 'es'}`;
}

/// The close of a ruled ticket that lands no harness file, run from the
/// person's own entry of what the ruling opened: tickets, or `nothing`.
async function close($: EngineInterface, row: Row, entered: string): Promise<void> {
  const said = entered.trim();
  const numbers = /^(?:#?\d+[\s,]*)+$/.test(said) ? (said.match(/\d+/g) ?? []) : [];
  const opens = /^nothing$/i.test(said) ? ['--opens-nothing'] : numbers.flatMap((n) => ['--opens', n]);
  if (opens.length === 0) return void $.ui.toast(`desk: #${row.n}: enter the tickets the ruling opened, as #3801 #3802, or nothing`);
  const session = await $.session.id();
  const args = ['ticket', 'close', String(row.n), ...opens, '--session-id', session];
  closing.set(row.n, `ticket close: ${(await xtaskLine($, args, /^(pass|fail|skip|xtask:|error)/)) ?? 'did not run'}`);
  $.ui.invalidate('ui.render');
}

/// Where this session's branch records a ticket the ruled one blocks and
/// nothing else open blocks it, a row the model reads says it is workable.
async function unblock($: EngineInterface, row: Row, cwd: string): Promise<void> {
  const track = await xtaskJson<{ ticket?: unknown }>($, ['track', '--json']);
  const waiting = typeof track?.ticket === 'number' ? track.ticket : undefined;
  if (waiting === undefined || !row.blocks.includes(waiting)) return;
  const viewed = await $.process.run(['gh', 'issue', 'view', String(waiting), '--json', 'blockedBy'], { cwd, timeoutMs: 60_000 }).catch(() => undefined);
  if (viewed?.exitCode !== 0) return;
  const blockedBy = parsed<{ blockedBy?: { nodes?: { number: number; state: string }[] } }>(viewed.stdout)?.blockedBy;
  if (blockedBy === undefined) return;
  const others = (blockedBy.nodes ?? []).filter((b) => b.state === 'OPEN' && b.number !== row.n);
  if (others.length > 0) return;
  await $.session.append({
    message: { type: 'user', content: text(`#${row.n} was ruled at the desk; #${waiting}, which it blocked, is workable.`) },
  });
}

export const register: Register = (on) => {
  on('session.start', async ($, e, next) => {
    const out = await next(e);
    for (const id of Object.keys(counselsOf(await $.store.get('counsels')))) counselIds.add(id);
    await $.command.register({ name: 'decide', description: "The desk: the for-a-person tickets waiting on a person's ruling" });
    const registered = await $.agent
      .register({
        name: COUNSEL,
        description: "Lays out the case for each option of one for-a-person ticket and drafts a ruling; the desk's alone",
        prompt: COUNSEL_PROMPT,
        tools: ['Read', 'Grep', 'Glob', 'Bash'],
        disallowedTools: ['Edit', 'Write', 'NotebookEdit'],
        model: 'claude-opus-5-5',
      })
      .catch((err: unknown) => void $.ui.log(`desk: the counsel type was not registered: ${String(err)}`));
    counselType = registered?.agent;
    // The first sweep a moment after the start, then one each period:
    // a sweep outlasts a hook's budget, so it runs on the clock.
    $.clock.after(1, () => void sweep($).catch(() => undefined));
    $.clock.every(EVERY_MS, () => void sweep($).catch(() => undefined));
    return out;
  });

  on('command.run', { command: 'decide' }, async ($) => {
    await $.ui.open({ id: PANE, title: 'Decisions' });
    return { text: 'The desk is open.' };
  });

  // The model is never offered the counsel: only the desk's own spawn runs it.
  on('agent.offer', (_$, e, next) => (e.agent === counselType ? { isOffered: false } : next(e))).catch((_$, e, next) =>
    e.agent === counselType ? { isOffered: false } : next(e),
  );

  // A counsel is the person's to talk to: a model's SendMessage to one is
  // refused, and refused too where the hook could not judge it.
  on('tool.call', { tool: 'SendMessage' }, async ($, e, next) =>
    (await isCounsel($, String(e.to ?? '')))
      ? { deny: "desk: a counsel is the person's to talk to, from /decide; no model sends it a message" }
      : next(e),
  ).catch((_$, e, next) =>
    next.called ? next(e) : { deny: 'desk: this SendMessage could not be held to the desk and is refused' },
  );

  // A counsel's own calls: the reads, and Bash held to the reads it may run.
  on('tool.call', async (_$, e, next) => {
    if (e.agentId === undefined || !counselIds.has(e.agentId)) return next(e);
    const deny = counselRefuses(e);
    return deny === undefined ? next(e) : { deny };
  }).catch((_$, e, next) =>
    next.called || e.agentId === undefined || !counselIds.has(e.agentId)
      ? next(e)
      : { deny: "desk: the counsel's call could not be judged and is refused" },
  );

  // A counsel's answer: its turn counted, its latest draft kept for Rule.
  on('turn.complete', async ($, e, next) => {
    const out = await next(e);
    if (e.agentId === undefined || !counselIds.has(e.agentId)) return out;
    const counsels = counselsOf(await $.store.get('counsels'));
    const was = counsels[e.agentId];
    if (was === undefined) return out;
    const draft = /^Draft ruling:\s*([\s\S]+)$/m.exec(e.answer)?.[1]?.trim();
    await $.store.set('counsels', { ...counsels, [e.agentId]: { ...was, turns: was.turns + 1, draft: draft ?? was.draft } });
    $.ui.invalidate('ui.render');
    return out;
  });

  // #2247's question after a ruling: the ruling it names is counted.
  on('tool.call', { tool: 'AskUserQuestion' }, async ($, e, next) => {
    const asked = JSON.stringify(e);
    const rulings = rulingsOf(await $.store.get('rulings'));
    if (rulings.some((r) => !r.confirmed && asked.includes(`#${r.n}`))) {
      await $.store.set('rulings', rulings.map((r) => (asked.includes(`#${r.n}`) ? { ...r, confirmed: true } : r)));
    }
    return next(e);
  }).catch((_$, e, next) => next(e));

  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const els = $.ui.resolve(e);
    const { Box, Text, Button } = els;
    const typing = e.surface !== 'mobile' && 'Input' in els && 'Select' in els;
    const session = await $.session.id();
    const rows = rowsOf(await $.store.get('queue')).filter((r) => !later.has(r.n));
    const counsels = Object.entries(counselsOf(await $.store.get('counsels'))).filter(([, c]) => c.session === session);
    const rulings = rulingsOf(await $.store.get('rulings'));
    const viewing = e.props.view.agentId !== undefined && counselIds.has(e.props.view.agentId);
    return (
      <Box flexDirection="column">
        {viewing && <Button key="back" onPress={() => void $.ui.view({})}>Back to the session</Button>}
        {swept.failed !== undefined && (
          <Text color="yellow">
            Stale: swept at {swept.at ?? 'no sweep of this session'}; {swept.failed}
          </Text>
        )}
        {rows.length === 0 && <Text dimColor>No decision is waiting.</Text>}
        {rows.map((row) => {
          const mine = counsels.filter(([, c]) => c.n === row.n).map(([, c]) => c);
          const turns = mine.reduce((sum, c) => sum + c.turns, 0);
          const draft = mine.map((c) => c.draft).filter((d) => d !== undefined).at(-1) ?? '';
          const options = row.options.map((label) => ({ value: label, label }));
          const open = ruling?.n === row.n ? ruling : undefined;
          const closable = ruled.has(row.n) && row.lands.length === 0 && !closing.get(row.n)?.startsWith('ticket close: pass');
          return (
            <Box flexDirection="column" marginBottom={1}>
              <Text bold>#{row.n} {row.title}</Text>
              <Text>{row.asks}</Text>
              <Text dimColor>Blocks: {row.blocks.length === 0 ? 'nothing open' : row.blocks.map((b) => `#${b}`).join(', ')}</Text>
              <Text dimColor>
                Lands: {row.lands.length === 0 ? 'no harness file; the ruling closes it' : `${row.lands.join(', ')}, by steer or #2247's question`}
              </Text>
              <Text dimColor>{row.criteria}</Text>
              {turns > 0 && (
                <Text dimColor>
                  Counsel: {turns} turn{turns === 1 ? '' : 's'}
                  {turns >= SOFT_CAP ? `, past the soft cap of ${SOFT_CAP}; Discuss continues` : ''}
                </Text>
              )}
              {outcome.has(row.n) && <Text>{outcome.get(row.n)}</Text>}
              {closing.has(row.n) && <Text>{closing.get(row.n)}</Text>}
              <Box flexDirection="row" gap={1}>
                <Button key={`discuss-${row.n}`} onPress={() => void discuss($, row)}>Discuss</Button>
                <Button
                  key={`rule-${row.n}`}
                  onPress={() => (
                    (ruling = { n: row.n, at: row.updatedAt, draft, typed: ruling?.n === row.n ? ruling.typed : undefined }),
                    $.ui.invalidate('ui.render')
                  )}
                >
                  Rule
                </Button>
                <Button key={`later-${row.n}`} onPress={() => (later.add(row.n), $.ui.invalidate('ui.render'))}>Not now</Button>
              </Box>
              {(open !== undefined || closable) && !typing && <Text dimColor>Rule from a terminal or the desktop app.</Text>}
              {open !== undefined && typing && (
                <Box flexDirection="column">
                  {options.length > 0 && (
                    <els.Select
                      key={`option-${row.n}`}
                      label="Option"
                      options={options}
                      value={open.option}
                      onSelect={(value: string) => ((ruling = { ...open, option: value, at: row.updatedAt }), $.ui.invalidate('ui.render'))}
                    />
                  )}
                  <els.Input
                    key={`words-${row.n}`}
                    label="Ruling"
                    value={open.typed ?? open.draft}
                    rows={4}
                    submitLabel="post"
                    onInput={(value: string) => void (ruling?.n === row.n && (ruling.typed = value))}
                    onSubmit={(value: string) => void rule($, row, value)}
                  />
                  <Button key={`cancel-${row.n}`} onPress={() => ((ruling = undefined), $.ui.invalidate('ui.render'))}>Cancel</Button>
                </Box>
              )}
              {closable && typing && (
                <els.Input
                  key={`opens-${row.n}`}
                  label="Opens"
                  placeholder="the tickets the ruling opened, as #3801 #3802, or nothing"
                  submitLabel="close"
                  onSubmit={(value: string) => void close($, row, value)}
                />
              )}
            </Box>
          );
        })}
        <Text dimColor>
          {rulings.length} ruled at the desk, {rulings.filter((r) => r.confirmed).length} later asked again by #2247's question
        </Text>
      </Box>
    );
  });
};
