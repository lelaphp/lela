// The desk's four parts against an engine this test plays: the tracker,
// xtask, git and the counsel's loop are stubs, so no test posts a ruling or
// closes a ticket (#3496). Each part has its test, and the two the ruling
// names apart: the model is never offered a counsel nor may message one,
// and a ruling is posted only from the person's own Input. A post under
// the person's name says what they chose (#3831): the option by its label,
// their typing kept, and a ticket changed since the pick posted nothing.
import type { AgentInfo, AgentSpec, On } from 'claude-code';
import { expect, mock, test, type Engine } from 'claude-code/testing';

const LISTED = [
  {
    number: 3765,
    title: "Whether php-cgi's release build carries the compiled tier",
    updatedAt: '2026-10-31T10:00:00Z',
    body: [
      '## What to decide',
      '',
      'Which php-cgi a user gets depends on how it was built.',
      '',
      '## Proposal for a person',
      '',
      '- `sapi/cgi` declares the **default** the other hosts do',
      '- `dist/Dockerfile` builds php-cgi on a line of its own',
      '',
      '## Acceptance criteria',
      '',
      '- [ ] A person rules',
      '- [ ] The module does what was ruled',
    ].join('\n'),
    blocking: { nodes: [{ number: 3496, state: 'OPEN' }, { number: 3000, state: 'CLOSED' }] },
  },
  {
    number: 3752,
    title: 'harness: no rule guards ~/.claude/settings.json',
    updatedAt: '2026-10-31T11:00:00Z',
    body: "## What to decide\n\nWhether harness.self's scope in seams.toml widens.\n",
    blocking: { nodes: [] },
  },
];

const PANE = {
  plugin: 'desk',
  component: 'Pane',
  requestId: 'decide',
  props: {
    title: 'Decisions',
    isFocused: true,
    bodyColumns: 100,
    placement: 'dock',
    scroll: { offset: 0, bodyRows: 60 },
    view: {},
  },
} as const;

const SURFACES = ['terminal', 'desktop'] as const;
const DECIDE = { command: 'decide', args: '', origin: { kind: 'composer' }, presentation: { isFullscreen: true, columns: 100 } } as const;
const ran = (stdout: string, exitCode = 0, stderr = '') => ({
  value: { exitCode, stdout, stderr, isStdoutTruncated: false, isStderrTruncated: false },
});
const answered = (agentId: string, answer: string, turnId = 't1') =>
  ({ answer, durationMs: 1, isAborted: false, turnId, agentId, reason: 'answer' }) as never;

/// The engine beneath the desk: the tracker and xtask answering as the
/// tickets above stand, as the real CLIs take their flags, a store, a
/// clock, and a record of every call that reaches the world: processes,
/// files, views, spawns, appended rows. `counsel` is the type the engine
/// names the desk's agent.
function beneath(on: On, counsel = 'desk:counsel') {
  const issues = structuredClone(LISTED);
  const saved = new Map<string, unknown>();
  const runs: { argv: readonly string[]; stdin?: string }[] = [];
  const writes: string[] = [];
  const statuses: (string | undefined)[] = [];
  const opened: string[] = [];
  const views: unknown[] = [];
  const spawns: [string, string][] = [];
  const agents: AgentSpec[] = [];
  const built = new Set<string>();
  const tracker = { list: undefined as string | undefined, comment: 'posts' as 'posts' | 'times out' | 'fails' };
  // What the band reads: `done`'s JSON, the tree git shows, the usage readings.
  const desk = { done: undefined as string | undefined, status: '# branch.oid 91ef0c0\n', diff: '', usage: '[]' };
  const clock = mock.clock(on, { now: Date.UTC(2026, 9, 31, 12) });
  const session = mock.session(on);
  on('session.start', (_$, e) => ({ cwd: e.cwd }));
  on('session.id', () => ({ value: '5e55100a-0000-4000' }));
  on('session.cwd', () => ({ value: '/work' }));
  on('store.get', (_$, e) => ({ value: saved.get(e.key) }));
  on('store.set', (_$, e) => (saved.set(e.key, e.value), { value: undefined }));
  // The engine may hand the hook the path under the session's directory.
  on('fs.exists', (_$, e) => ({ value: built.has(e.path) || built.has(e.path.replace(/^\/work\//, '')) }));
  on('fs.write', (_$, e) => (writes.push(e.path), { value: undefined }));
  on('command.register', (_$, e) => ({ value: { command: e.name } }));
  on('agent.register', (_$, e) => (agents.push(e), { value: { agent: counsel } }));
  const listed: AgentInfo[] = [];
  const down = { list: false };
  on('agent.list', () => (down.list ? { deny: 'the agent list is down' } : { value: listed }));
  // The kit hands a spawn's bottom the Agent tool's input and keeps the id
  // for the engine to set, so the counsel is found where the desk looks
  // next: the agent list.
  on('agent.spawn', (_$, e) => {
    const call = e as unknown as { subagent_type: string; description: string };
    spawns.push([call.subagent_type, call.description]);
    listed.push({ id: `counsel-${spawns.length}`, description: call.description, type: call.subagent_type, status: 'running' });
    return { model: 'claude-opus-5-5' };
  });
  on('ui.open', (_$, e) => (opened.push(e.id), { value: { isPlaced: true } }));
  on('ui.status', (_$, e) => (statuses.push(e.text), { value: undefined }));
  on('ui.view', (_$, e) => (views.push(e), { value: {} }));
  on('ui.toast', () => ({ value: undefined }));
  on('turn.complete', () => ({ text: '' }));
  on('tool.call', () => ({ result: 'ran' }));
  let stamp = 0;
  const touched = (issue: (typeof issues)[number]) => (issue.updatedAt = `2026-10-31T12:0${(stamp += 1)}:00Z`);
  on('process.run', (_$, e) => {
    runs.push({ argv: e.argv, stdin: e.init?.stdin });
    const line = e.argv.join(' ');
    const issue = issues.find((i) => i.number === Number(e.argv[3]));
    if (line.startsWith('gh issue list')) return ran(tracker.list ?? JSON.stringify(issues));
    if (line.startsWith('gh issue comment') && issue !== undefined) {
      // GitHub moves a ticket's updatedAt on a comment, whatever the client saw.
      if (tracker.comment !== 'fails') touched(issue);
      if (tracker.comment === 'times out') return { deny: 'gh issue comment timed out after 60000 ms' };
      return tracker.comment === 'fails' ? ran('', 1, 'HTTP 502: Bad Gateway') : ran(`https://github.com/x/y/issues/${issue.number}#c1\n`);
    }
    if (line.startsWith('gh issue edit') && issue !== undefined) return (issue.body = e.init?.stdin ?? '', touched(issue), ran(''));
    if (line.startsWith('gh issue view 3496')) return ran('{"blockedBy":{"nodes":[{"number":3765,"state":"OPEN"}]}}');
    if (line.startsWith('gh issue view') && issue !== undefined) return ran(JSON.stringify({ updatedAt: issue.updatedAt, body: issue.body }));
    if (line.includes('ticket read')) return ran(`fail ticket ${e.argv.at(-1)}: 0 of 1 criteria ticked\n`, 1);
    if (line.includes('ticket close')) return ran(`fail ticket 3765: 1 of 2 criteria ticked\n`, 1);
    if (line.endsWith(' done --format json --peek') && desk.done !== undefined) return ran(`${desk.done}\n`, 1, 'xtask: 5 conditions of done unmet\n');
    if (line.endsWith(' status --section usage --format json')) return ran(desk.usage);
    if (line.startsWith('git --no-optional-locks status')) return ran(desk.status);
    if (line.startsWith('git --no-optional-locks diff')) return ran(desk.diff);
    // xtask's own flags: `track --json`, and an unknown one refused as clap does.
    if (/(?:^|\/| -- )xtask track --json$|^cargo run -q -p xtask -- track --json$/.test(line)) return ran('{"base":"7a8ddbc","branch":"t3496","ticket":3496}\n');
    if (line.includes(' track ')) return ran('', 2, "error: unexpected argument '--format' found\n");
    if (line === 'git config user.name') return ran('Mitchell van Wijngaarden\n');
    return ran('');
  });
  const posts = () => runs.filter((r) => r.argv.join(' ').startsWith('gh issue comment'));
  return { down, listed, saved, runs, writes, statuses, opened, views, spawns, agents, clock, session, posts, issues, tracker, built, desk };
}

/// A session started and its first sweep run.
async function started($: Engine, world: ReturnType<typeof beneath>) {
  await $.session.start({ cwd: '/work', surface: 'terminal', isInteractive: true });
  await world.clock.advance(1);
}

test('the queue keeps the for-a-person tickets in the store and counts them, opening no pane', async ($, on) => {
  const world = beneath(on);
  await started($, world);
  expect(world.statuses.at(-1)).toBe('harness: 2 decisions waiting');
  expect(world.opened).toEqual([]);
  const [first, second] = world.saved.get('queue') as Record<string, unknown>[];
  expect(first).toEqual({
    n: 3765,
    title: "Whether php-cgi's release build carries the compiled tier",
    asks: 'Which php-cgi a user gets depends on how it was built.',
    options: ['`sapi/cgi` declares the default the other hosts do', '`dist/Dockerfile` builds php-cgi on a line of its own'],
    blocks: [3496],
    lands: [],
    criteria: 'fail ticket 3765: 0 of 1 criteria ticked',
    updatedAt: '2026-10-31T10:00:00Z',
  });
  expect(second?.lands).toEqual(['seams.toml']);
  const reads = () => world.runs.filter((r) => r.argv.includes('read')).length;
  expect(reads()).toBe(2);
  // The next sweep reads no ticket the tracker has not changed.
  await world.clock.advance(5 * 60_000);
  expect(world.runs.filter((r) => r.argv[0] === 'gh' && r.argv[2] === 'list').length).toBe(2);
  expect(reads()).toBe(2);
});

test('a sweep that fails says the queue is stale, in the status line and the pane', async ($, on) => {
  const world = beneath(on);
  await started($, world);
  world.tracker.list = '<html>rate limited</html>';
  await world.clock.advance(5 * 60_000);
  expect(world.statuses.at(-1)).toBe('harness: 2 decisions waiting, stale since 12:00 UTC');
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' });
  expect((await ui.find({ type: 'Text', text: /^Stale/ }))?.text).toMatch(/^Stale: swept at 12:00 UTC; the sweep at 12:05 UTC failed: gh issue list answered no JSON/);
  expect(await ui.find({ key: 'discuss-3765' })).toBeDefined();
  await ui.unmount();
});

test('/decide opens the pane: per ticket what it asks, what it blocks, where it lands, and three presses', async ($, on) => {
  const world = beneath(on);
  await started($, world);
  expect((await $.command.run(DECIDE)).text).toBe('The desk is open.');
  expect(world.opened).toEqual(['decide']);
  for (const surface of SURFACES) {
    const ui = await $.ui.mount({ ...PANE, surface });
    expect((await ui.find({ type: 'Text', text: /^#3765 / }))?.text).toBe("#3765 Whether php-cgi's release build carries the compiled tier");
    expect(await ui.find({ type: 'Text', text: 'Which php-cgi a user gets' })).toBeDefined();
    expect((await ui.find({ type: 'Text', text: /^Blocks: #3496/ }))?.text).toBe('Blocks: #3496');
    expect(await ui.find({ type: 'Text', text: 'Lands: no harness file; the ruling closes it' })).toBeDefined();
    expect(await ui.find({ type: 'Text', text: "Lands: seams.toml, by steer or #2247's question" })).toBeDefined();
    for (const key of ['discuss-3765', 'rule-3765', 'later-3765']) expect(await ui.find({ key })).toBeDefined();
    await ui.unmount();
  }
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' });
  await ui.press({ key: 'later-3765' });
  expect(await ui.find({ key: 'discuss-3765' })).toBe(undefined);
  expect(await ui.find({ key: 'discuss-3752' })).toBeDefined();
  await ui.unmount();
});

test('the counsel is read-only Opus, and its Bash is held to the reads it may run', async ($, on) => {
  const world = beneath(on);
  await started($, world);
  const [spec] = world.agents;
  expect(spec?.name).toBe('counsel');
  expect(spec?.model).toBe('claude-opus-5-5');
  expect(spec?.tools).toEqual(['Read', 'Grep', 'Glob', 'Bash']);
  expect(spec?.prompt).toMatch(/never do/);
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' });
  await ui.press({ key: 'discuss-3765' });
  expect(world.spawns).toEqual([['desk:counsel', '#3765']]);
  expect(world.views).toEqual([{ agentId: 'counsel-1' }]);
  const call = (command: string) => $.tool.call({ tool: 'Bash', command, agentId: 'counsel-1' });
  const reads = ['cargo xtask status --section phpt', 'target/debug/xtask ticket read 3765', './target/release/xtask status', 'git log -3', 'git show HEAD', 'git diff main', 'gh issue view 3765 --comments'];
  for (const command of reads) expect((await call(command)).deny).toBe(undefined);
  const refused = ['git log; rm x', 'git diff > x', 'git log --output=x', 'gh issue comment 3765 -b x', 'cargo xtask steer seams.toml', 'cargo xtask ticket close 3765', 'echo $(id)'];
  // An xtask by any other path is whatever stands there: only the build and cargo's are the harness's.
  refused.push('xtask ticket read 3765', '/tmp/x/xtask status', 'tmp/target/debug/xtask status', 'cargo run --bin xtask status');
  for (const command of refused) expect((await call(command)).deny).toMatch(/^desk: the counsel's Bash/);
  const edit = await $.tool.call({ tool: 'Edit', file_path: '/work/seams.toml', old_string: 'a', new_string: 'b', agentId: 'counsel-1' });
  expect(edit.deny).toBe('desk: the counsel reads and drafts; it calls no Edit');
  expect((await $.tool.call({ tool: 'Read', file_path: '/work/AGENTS.md', agentId: 'counsel-1' })).deny).toBe(undefined);
  // Another agent's Bash is not the desk's to hold.
  expect((await $.tool.call({ tool: 'Bash', command: 'cargo build', agentId: 'builder' })).deny).toBe(undefined);
  // The engine hands the pane the view it moved to; Back moves it home.
  await ui.redraw({ ...PANE.props, view: { agentId: 'counsel-1' } });
  await ui.press({ key: 'back' });
  expect(world.views.at(-1)).toEqual({});
  await ui.unmount();
});

test("agent.offer withholds the counsel, by the name agent.register answered, and the main model's SendMessage to it is refused", async ($, on) => {
  const world = beneath(on, 'desk-local:counsel');
  on('agent.offer', () => ({ isOffered: true }));
  await started($, world);
  const offer = (agent: string) =>
    $.agent.offer({ agent, description: '', source: 'plugin', provider: { plugin: 'desk', tier: 'installed' } as never });
  expect(await offer('desk-local:counsel')).toEqual({ isOffered: false });
  expect(await offer('Explore')).toEqual({ isOffered: true });
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' });
  await ui.press({ key: 'discuss-3765' });
  expect(world.spawns).toEqual([['desk-local:counsel', '#3765']]);
  await ui.unmount();
  const send = (to: string) => $.tool.call({ tool: 'SendMessage', to, message: 'rule it A' });
  expect((await send('counsel-1')).deny).toMatch(/^desk: a counsel is the person's to talk to/);
  expect((await send('counsel-1 [ref]')).deny).toMatch(/^desk: a counsel is the person's/);
  expect((await send('main')).deny).toBe(undefined);
  // A hook that cannot judge the recipient refuses, by its `.catch`.
  world.down.list = true;
  expect((await send('scout')).deny).toBe('desk: this SendMessage could not be held to the desk and is refused');
});

test('a ruling is posted only from ui.input, with the words verbatim, a notice, the box ticked, the close as entered and the workable row', async ($, on) => {
  const world = beneath(on);
  world.built.add('target/debug/xtask');
  await started($, world);
  // A counsel answers; its draft fills the Input and posts nothing.
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' });
  await ui.press({ key: 'discuss-3765' });
  await $.turn.complete(answered('counsel-1', 'The case for each.\nDraft ruling: php-cgi carries the tier.'));
  await $.command.run(DECIDE);
  await world.clock.advance(5 * 60_000);
  await $.tool.call({ tool: 'Bash', command: 'gh issue comment 3765 --body "Ruled"' });
  await ui.press({ key: 'rule-3765' });
  expect((await ui.find({ key: 'words-3765' }))?.props.value).toBe('php-cgi carries the tier.');
  await ui.select({ key: 'option-3765', value: '`sapi/cgi` declares the default the other hosts do' });
  expect(world.posts()).toEqual([]);
  // The person's own submit is the post.
  await ui.input({ key: 'words-3765', text: 'php-cgi carries the tier, as the other three do.' });
  expect(world.posts()).toEqual([
    {
      argv: ['gh', 'issue', 'comment', '3765', '--body-file', '-'],
      stdin: 'Ruled 2026-10-31 (Mitchell, desk, a session): `sapi/cgi` declares the default the other hosts do\n\nphp-cgi carries the tier, as the other three do.',
    },
  ]);
  // The "A person rules" box alone is ticked, its text unchanged.
  const edits = world.runs.filter((r) => r.argv.join(' ') === 'gh issue edit 3765 --body-file -');
  expect(edits.map((r) => r.stdin)).toEqual([LISTED[0]!.body.replace('- [ ] A person rules', '- [x] A person rules')]);
  // The session waiting on #3496 hears it, through xtask's own `track --json`.
  // Whichever candidate ran it, built or cargo's, the flag is the real one.
  expect(world.runs.some((r) => / track --json$/.test(r.argv.join(' ')))).toBe(true);
  const rows = world.session.appended().map((r) => [r.message.type, r.message.content]);
  expect(rows).toEqual([
    ['system', [{ type: 'text', text: 'desk: #3765 ruled by Mitchell and posted to the ticket' }]],
    ['user', [{ type: 'text', text: '#3765 was ruled at the desk; #3496, which it blocked, is workable.' }]],
  ]);
  // No close is run until the person says what the ruling opened.
  expect(world.runs.some((r) => r.argv.includes('close'))).toBe(false);
  expect((await ui.find({ type: 'Text', text: /^ruled and posted/ }))?.text).toBe('ruled and posted; ticked 1 "A person rules" box');
  await ui.input({ key: 'opens-3765', text: 'a few' });
  expect(world.runs.some((r) => r.argv.includes('close'))).toBe(false);
  await ui.input({ key: 'opens-3765', text: '#3801, #3802' });
  // The arguments from `ticket` on, whichever xtask candidate ran them.
  const closes = world.runs.filter((r) => r.argv.includes('close')).map((r) => r.argv.slice(r.argv.indexOf('ticket')).join(' '));
  expect(closes).toEqual(['ticket close 3765 --opens 3801 --opens 3802 --session-id 5e55100a-0000-4000']);
  // The close refuses while the other box stands, and the pane says so.
  expect((await ui.find({ type: 'Text', text: /^ticket close/ }))?.text).toBe('ticket close: fail ticket 3765: 1 of 2 criteria ticked');
  // A second submit posts nothing more, and the desk wrote no file.
  await ui.press({ key: 'rule-3765' });
  await ui.input({ key: 'words-3765', text: 'again' });
  expect(world.posts().length).toBe(1);
  expect(world.writes).toEqual([]);
  await ui.unmount();
});

test('a pick is kept by its label: options reordered at the tracker after it post nothing, or the label picked', async ($, on) => {
  const world = beneath(on);
  await started($, world);
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' });
  await ui.press({ key: 'rule-3765' });
  const select = await ui.find({ key: 'option-3765' });
  const [picked] = select?.props.options as { value: string; label: string }[];
  expect(picked?.label).toBe('`sapi/cgi` declares the default the other hosts do');
  await ui.select({ key: 'option-3765', value: picked!.value });
  // Someone edits the ticket and reverses its options; a sweep draws them.
  const issue = world.issues[0]!;
  const [a, b] = ['- `sapi/cgi` declares the **default** the other hosts do', '- `dist/Dockerfile` builds php-cgi on a line of its own'];
  issue.body = issue.body.replace(`${a}\n${b}`, `${b}\n${a}`);
  issue.updatedAt = '2026-10-31T12:03:00Z';
  await world.clock.advance(5 * 60_000);
  await ui.input({ key: 'words-3765', text: 'As picked.' });
  expect(world.posts().filter((p) => !p.stdin?.includes(picked!.label))).toEqual([]);
  expect(world.posts()).toEqual([]);
  expect((await ui.find({ type: 'Text', text: /^not posted/ }))?.text).toBe(
    'not posted: #3765 changed at the tracker since the pick (2026-10-31T10:00:00Z, now 2026-10-31T12:03:00Z); read it and pick again',
  );
  // The words stay; a pick from the options as they now stand posts its own label.
  await ui.press({ key: 'rule-3765' });
  expect((await ui.find({ key: 'words-3765' }))?.props.value).toBe('As picked.');
  await ui.select({ key: 'option-3765', value: picked!.value });
  await ui.input({ key: 'words-3765', text: 'As picked.' });
  expect(world.posts().map((p) => p.stdin)).toEqual([`Ruled 2026-10-31 (Mitchell, desk, a session): ${picked!.label}\n\nAs picked.`]);
  await ui.unmount();
});

test("a counsel turn that ends while the person types keeps their words; the counsel's draft posted unedited is marked", async ($, on) => {
  const world = beneath(on);
  await started($, world);
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' });
  await ui.press({ key: 'discuss-3765' });
  await $.turn.complete(answered('counsel-1', 'Draft ruling: php-cgi carries the tier.'));
  await ui.press({ key: 'rule-3765' });
  await ui.input({ key: 'words-3765', text: 'php-cgi carries it, as', kind: 'change' });
  await $.turn.complete(answered('counsel-1', 'On reflection.\nDraft ruling: php-cgi does not carry the tier.', 't2'));
  expect((await ui.find({ key: 'words-3765' }))?.props.value).toBe('php-cgi carries it, as');
  await ui.input({ key: 'words-3765', text: 'php-cgi carries it, as the others do.' });
  expect(world.posts().map((p) => p.stdin)).toEqual(['Ruled 2026-10-31 (Mitchell, desk, a session): php-cgi carries it, as the others do.']);
  // The draft frozen at Rule is the Input's, and posted as it stands it says whose words they are.
  await ui.press({ key: 'discuss-3752' });
  await $.turn.complete(answered('counsel-2', 'Draft ruling: widen it.', 't3'));
  await ui.press({ key: 'rule-3752' });
  await ui.input({ key: 'words-3752', text: 'widen it.' });
  expect(world.posts().at(-1)?.stdin).toBe("Ruled 2026-10-31 (Mitchell, desk, a session; the counsel's draft, unedited): widen it.");
  await ui.unmount();
});

test('a post that fails or times out is shown, and never sent again blind', async ($, on) => {
  const world = beneath(on);
  await started($, world);
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' });
  await ui.press({ key: 'rule-3752' });
  world.tracker.comment = 'fails';
  await ui.input({ key: 'words-3752', text: 'Widen it.' });
  expect((await ui.find({ type: 'Text', text: /^not posted/ }))?.text).toBe('not posted: gh exited 1: HTTP 502: Bad Gateway');
  // A comment that timed out may have landed: the retry is held to the ticket as picked, and refused.
  world.tracker.comment = 'times out';
  await ui.input({ key: 'words-3752', text: 'Widen it.' });
  expect((await ui.find({ type: 'Text', text: /^not known/ }))?.text).toMatch(/^not known whether the ruling posted: .*timed out/);
  world.tracker.comment = 'posts';
  await ui.input({ key: 'words-3752', text: 'Widen it.' });
  expect(world.posts().length).toBe(2);
  expect((await ui.find({ type: 'Text', text: /^not posted/ }))?.text).toMatch(/^not posted: #3752 changed at the tracker since the pick/);
  await ui.unmount();
});

test('a ruling that lands a harness file closes nothing, and a later #2247 question naming it is counted', async ($, on) => {
  const world = beneath(on);
  await started($, world);
  const ui = await $.ui.mount({ ...PANE, surface: 'desktop' });
  await ui.press({ key: 'rule-3752' });
  await ui.input({ key: 'words-3752', text: 'Widen it.' });
  expect(world.posts().map((p) => p.stdin)).toEqual(['Ruled 2026-10-31 (Mitchell, desk, a session): Widen it.']);
  expect(world.runs.some((r) => r.argv.includes('close'))).toBe(false);
  expect(await ui.find({ key: 'opens-3752' })).toBe(undefined);
  expect(world.session.appended().map((r) => r.message.type)).toEqual(['system']);
  await $.tool.call({ tool: 'AskUserQuestion', questions: [{ question: 'Land #3752 ruling in seams.toml?' }] } as never);
  expect(world.saved.get('rulings')).toEqual([{ n: 3752, at: '2026-10-31T12:00:00.001Z', session: '5e55100a-0000-4000', confirmed: true }]);
  await ui.unmount();
});

test('past twenty turns the row says the soft cap is passed, and Discuss continues', async ($, on) => {
  const world = beneath(on);
  await started($, world);
  const ui = await $.ui.mount({ ...PANE, surface: 'terminal' });
  await ui.press({ key: 'discuss-3765' });
  for (let turn = 0; turn < 20; turn += 1) await $.turn.complete(answered('counsel-1', 'More.', `t${turn}`));
  expect((await ui.find({ type: 'Text', text: /^Counsel: / }))?.text).toBe('Counsel: 20 turns, past the soft cap of 20; Discuss continues');
  await ui.press({ key: 'discuss-3765' });
  expect(world.spawns.length).toBe(1);
  expect(world.views).toEqual([{ agentId: 'counsel-1' }, { agentId: 'counsel-1' }]);
  await ui.unmount();
});

/// `done --format json --peek` as t4251's build printed it on a ticket's
/// worktree, with the four kinds its test pins (#3494).
const DONE = {
  v: 2,
  base: { commit: '91ef0c0ed9629f22e3a81b165d20ac412fc6fd28', since: 'main' },
  changed: ['ext/pcre/src/lib.rs'],
  counts: { failing: 2, stale: 1, missing: 1, refused: 1, unkinded: 0 },
  crates: ['ext/pcre'],
  delegated: [],
  met: false,
  notes: [],
  ticket: 3497,
  unmet: [
    { command: 'cargo xtask clippy --crate ext/pcre', instrument: 'clippy', kind: 'failing', message: 'clippy: ext/pcre fails', scope: 'ext/pcre' },
    { command: 'cargo xtask test --crate ext/pcre', instrument: 'test', kind: 'stale', message: 'test: ext/pcre is stale; its last reading failed', scope: 'ext/pcre' },
    { command: null, instrument: null, kind: 'refused', message: 'open divergence pcre-jit on ext/pcre', scope: null },
    { command: 'cargo xtask witness --crate ext/pcre', instrument: 'witness', kind: 'missing', message: 'witness: no reading for ext/pcre', scope: 'ext/pcre' },
    {
      command: 'cargo xtask ticket read',
      instrument: 'ticket',
      kind: 'failing',
      message: 'ticket: #3497: 1 of 3 criteria ticked; unticked: (2) The band does not run `done`; twice; (3) The status line shows the spend',
      scope: '3497',
    },
  ],
};
const ABOVE = {
  plugin: 'desk',
  component: 'AbovePrompt',
  props: { hasSurvey: false, isCollapsed: false, isWorking: false, maxRows: 12, bodyColumns: 100, scroll: { offset: 0, bodyRows: 11 }, view: {} },
} as const;
const mainTurn = (turnId: string) => answered(undefined as never, 'Done.', turnId);

test('the band draws done --format json --peek: a count, then the conditions and the unticked boxes, folded to the count, and writes no reading', async ($, on) => {
  const world = beneath(on);
  world.tracker.list = '[]';
  world.desk.done = JSON.stringify(DONE);
  await started($, world);
  for (const surface of SURFACES) {
    const ui = await $.ui.mount({ ...ABOVE, surface });
    const shown = async () => (await ui.findAll({ type: 'Text' })).map((t) => t.text);
    const head = 'done: 5 unmet since main (2 failing, 1 stale, 1 missing, 1 refused); #3497: 2 boxes unticked';
    expect(await shown()).toEqual([head, 'failing: clippy: ext/pcre fails', '+5 more; cargo xtask done lists them']);
    await ui.press({ key: 'band-fold' });
    expect(await shown()).toEqual([head]);
    await ui.press({ key: 'band-fold' });
    await ui.unmount();
  }
  // The boxes, each its own row where there is room, a `;` inside one kept.
  world.desk.done = JSON.stringify({ ...DONE, unmet: DONE.unmet.slice(4), counts: { failing: 1 } });
  await $.tool.call({ tool: 'Bash', command: 'cargo xtask ticket read' });
  await world.clock.advance(1);
  const ui = await $.ui.mount({ ...ABOVE, surface: 'terminal' });
  expect((await ui.findAll({ type: 'Text' })).map((t) => t.text).slice(1)).toEqual(['#3497 (2) The band does not run `done`; twice', '#3497 (3) The status line shows the spend']);
  await ui.unmount();
  // Each xtask the band ran reads: `done` peeked, and the usage readings.
  const asked = world.runs.map((r) => r.argv.join(' ')).filter((l) => l.includes('xtask')).map((l) => l.replace(/^.*?xtask (?:-- )?/, ''));
  expect([...new Set(asked)]).toEqual(['done --format json --peek', 'status --section usage --format json']);
  expect(world.runs.filter((r) => r.argv[0] === 'git').every((r) => r.argv[1] === '--no-optional-locks')).toBe(true);
  expect(world.writes).toEqual([]);
});

test('the band asks done again only where the fingerprint moved or a Bash call ran an instrument', async ($, on) => {
  const world = beneath(on);
  // Beneath the desk, the engine's own band: an empty one, reached once the desk passes.
  let passed = 0;
  on('ui.render', { component: 'AbovePrompt' }, () => ((passed += 1), h('Box', null) as never));
  world.desk.done = JSON.stringify(DONE);
  await started($, world);
  const peeks = () => world.runs.filter((r) => r.argv.includes('--peek')).length;
  const turn = async (id: string) => ($.turn.complete(mainTurn(id)), world.clock.advance(1));
  expect(peeks()).toBe(1);
  await turn('t1');
  expect(peeks()).toBe(1);
  // A read runs no instrument.
  await $.tool.call({ tool: 'Bash', command: 'cargo xtask status --section phpt' });
  await world.clock.advance(1);
  expect(peeks()).toBe(1);
  world.desk.diff = 'diff --git a/ext/pcre/src/lib.rs b/ext/pcre/src/lib.rs\n+pub fn preg_split() {}\n';
  await turn('t2');
  expect(peeks()).toBe(2);
  await turn('t3');
  expect(peeks()).toBe(2);
  // A commit moves HEAD with no diff left.
  [world.desk.diff, world.desk.status] = ['', '# branch.oid 7a8ddbc\n'];
  await turn('t4');
  expect(peeks()).toBe(3);
  // An instrument over the same tree is asked again, and its list redrawn: met, the band passes.
  world.desk.done = JSON.stringify({ ...DONE, met: true, unmet: [], counts: {} });
  await $.tool.call({ tool: 'Bash', command: 'target/debug/xtask witness --crate ext/pcre' });
  await world.clock.advance(1);
  expect(peeks()).toBe(4);
  const ui = await $.ui.mount({ ...ABOVE, surface: 'desktop' });
  expect([passed, (await ui.findAll({ type: 'Text' })).length]).toEqual([1, 0]);
  await ui.unmount();
});

test("the status line shows the branch ticket's spend, from its last usage reading, beside the queue's count", async ($, on) => {
  const world = beneath(on);
  world.desk.done = JSON.stringify(DONE);
  const usage = (usd: number, turn: number) =>
    JSON.stringify([
      { scope: '3496', reading: { instrument: 'usage', numbers: { usd: 99, turn: 3 } } },
      { scope: '3497', reading: { instrument: 'usage', numbers: { usd, turn, tokens: 152_000 } } },
    ]);
  world.desk.usage = usage(3.2146, 14);
  await started($, world);
  expect(world.statuses.at(-1)).toBe('harness: 2 decisions waiting · #3497: $3.21 spent by turn 14');
  // A turn whose tree did not move reads the spend again, and not done.
  world.desk.usage = usage(4.5, 15);
  await $.turn.complete(mainTurn('t1'));
  await world.clock.advance(1);
  expect(world.statuses.at(-1)).toBe('harness: 2 decisions waiting · #3497: $4.50 spent by turn 15');
  expect(world.runs.filter((r) => r.argv.includes('--peek')).length).toBe(1);
});
