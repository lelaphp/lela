// The function hook hands permit the person's answers (#2247) only where
// a denial an approval could lift asks for them, and only the engine's
// answered AskUserQuestion records; the engine beneath is this test's. A
// denial is kept in $.store, and an Edit or a Write no judge saw is a deny,
// whether no candidate judged or the hook threw (#3489). A spawn whose
// first line is `/build <n>` starts a builder from its brief, in its
// worktree, or is refused; xtask runs from the session root (#4270).
import type { AgentSpawnArgs, AgentSpawnInput, On } from 'claude-code';
import type { Engine } from 'claude-code/testing';
import { expect, mock, test } from 'claude-code/testing';

const ANSWERED = { tool_use_id: 'toolu_ask', tool: 'AskUserQuestion', input: { q: 1 }, result: { answers: {} } };
const UNANSWERED = { tool_use_id: 'toolu_open', tool: 'AskUserQuestion', input: {} };
const DENY = '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"By a person. (harness.self)"}}';
const LOCK = '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"No lock. (ext.no-locks, ADR-0005)"}}';
const WRITE = { tool: 'Write', file_path: '/work/ext/a.rs', content: '' } as const;
const ran = (stdout: string, exitCode = 0) => ({
  value: { exitCode, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false },
});

/// The engine beneath a judged call: permit answering `run`, a store the
/// test reads, a clock at the ruling's day, and a tool that runs.
function beneath(on: On, run: ReturnType<typeof ran> | { deny: string }): Map<string, unknown> {
  const saved = new Map<string, unknown>();
  on('session.id', () => ({ value: 'session' }));
  on('session.cwd', () => ({ value: '/work' }));
  on('session.root', () => ({ value: '/work' }));
  on('fs.exists', () => ({ value: false }));
  on('process.run', () => run);
  on('store.get', (_$, e) => ({ value: saved.get(e.key) }));
  on('store.set', (_$, e) => (saved.set(e.key, e.value), { value: undefined }));
  mock.clock(on, { now: Date.UTC(2026, 9, 25) });
  on('tool.call', () => ({ result: { filePath: WRITE.file_path } }));
  return saved;
}

test('an Edit or a Write no candidate judged is denied, and Bash handed on', async ($, on) => {
  beneath(on, ran('', 1));
  expect((await $.tool.call(WRITE)).deny).toMatch(/^xtask permit did not judge this call/);
  expect((await $.tool.call({ tool: 'Bash', command: 'ls' })).deny).toBe(undefined);
});

test('a throw before permit judged denies an Edit or a Write, and hands Bash on', async ($, on) => {
  beneath(on, { deny: 'timed out' });
  expect((await $.tool.call(WRITE)).deny).toMatch(/before xtask permit judged this Write/);
  expect((await $.tool.call({ tool: 'Bash', command: 'ls' })).deny).toBe(undefined);
});

test('a denial is kept in the store as its rule, ADR, tool and time', async ($, on) => {
  const saved = beneath(on, ran(LOCK));
  await $.tool.call(WRITE);
  expect(saved.get('denials')).toEqual([
    { rule: 'ext.no-locks', adr: 'ADR-0005', tool: 'Write', at: '2026-10-25T00:00:00.000Z' },
  ]);
});

test('a harness.self denial is asked again with the answers, and lifted', async ($, on) => {
  const stdins: Record<string, unknown>[] = [];
  on('session.id', () => ({ value: 'session' }));
  on('session.cwd', () => ({ value: '/work' }));
  on('session.root', () => ({ value: '/work' }));
  on('session.messages', () => ({
    value: [{ role: 'assistant', text: 'User has answered', toolUses: [ANSWERED, UNANSWERED] }],
  }));
  on('fs.exists', () => ({ value: false }));
  on('process.run', (_$, e) => {
    const stdin = JSON.parse(String(e.init?.stdin));
    stdins.push(stdin);
    return ran(stdin.asked === undefined ? DENY : '');
  });
  on('tool.call', { tool: 'Edit' }, () => ({ result: { filePath: '/work/seams.toml' } }));
  const out = await $.tool.call({ tool: 'Edit', file_path: '/work/seams.toml', old_string: 'a', new_string: 'b' });
  expect(out.deny).toBe(undefined);
  expect(stdins.length).toBe(2);
  expect(stdins[0].asked).toBe(undefined);
  expect(stdins[1].asked).toEqual([{ tool_use_id: 'toolu_ask', input: { q: 1 }, result: { answers: {} } }]);
});

test("prompt.context hands over the facts alone, the map being CLAUDE.md's load", async ($, on) => {
  on('session.id', () => ({ value: 'session' }));
  on('session.cwd', () => ({ value: '/work' }));
  on('session.root', () => ({ value: '/work' }));
  on('process.run', () => ran(JSON.stringify({ map: '# The map\n', facts: '# Local facts\n' })));
  on('prompt.context', (_$, e) => ({ blocks: e.blocks }));
  const { blocks } = await $.prompt.context({
    blocks: [
      { name: 'claudeMd', text: '@AGENTS.md' },
      { name: 'harness-map', text: 'a map block from before' },
    ],
  });
  expect(blocks.map((b) => b.name)).toEqual(['claudeMd', 'harness-facts']);
  expect(blocks[1]?.text).toBe('# Local facts\n');
});

/// A brief as `xtask ticket brief` prints one, naming its worktree.
const BRIEF = '## Brief\n\nYou are the builder for #4270.\n\nWorktree: /work-worktrees/t4270, on branch t4270, source main, laid on main.\n';
const SPAWNED = { model: 'claude-opus-5-5', agentId: 'agent-1' };

/// A spawn as the Agent tool's caller or a workflow gives one; the engine
/// fills the rest.
const spawn = ($: Engine, e: AgentSpawnArgs & Partial<AgentSpawnInput>) =>
  $.agent.spawn(e as AgentSpawnInput);

/// The engine beneath a spawn: a session at `/work` whose subagent stands
/// in `/work/sub`, a debug build under the root, `ticket brief` answering
/// `brief` (a function of the argv), and the spawn that reached core kept;
/// `root` answers the session root.
function spawning(
  on: On,
  brief: (argv: readonly string[]) => ReturnType<typeof ran>,
  root: () => { value: string } = () => ({ value: '/work' }),
) {
  const seen = { runs: [] as { argv: readonly string[]; cwd?: string }[], spawned: [] as AgentSpawnInput[] };
  on('session.id', () => ({ value: 'session' }));
  on('session.cwd', () => ({ value: '/work/sub' }));
  on('session.root', root);
  on('fs.exists', (_$, e) => ({ value: e.path === '/work/target/debug/xtask' }));
  on('process.run', (_$, e) => {
    seen.runs.push({ argv: e.argv, cwd: e.init?.cwd });
    return brief(e.argv);
  });
  on('agent.spawn', (_$, e) => (seen.spawned.push(e), SPAWNED));
  return seen;
}

test('/build <n> alone on the first line is the brief, the rest under a coordinator heading, a builder in its worktree', async ($, on) => {
  const seen = spawning(on, () => ran(BRIEF));
  const out = await spawn($, { prompt: '/build 4270\nOwn harness.ts.\n', description: '#4270' });
  expect(out.deny).toBe(undefined);
  expect(seen.runs).toEqual([{ argv: ['/work/target/debug/xtask', 'ticket', 'brief', '4270'], cwd: '/work' }]);
  expect(seen.spawned.length).toBe(1);
  expect(seen.spawned[0]?.prompt).toBe(`${BRIEF}\n## Coordinator\n\nOwn harness.ts.\n`);
  expect(seen.spawned[0]?.subagentType).toBe('builder');
  expect(seen.spawned[0]?.cwd).toBe('/work-worktrees/t4270');
});

test('a stale build that will not render the brief is passed over for cargo, run at the root', async ($, on) => {
  const seen = spawning(on, (argv) => (argv[0] === 'cargo' ? ran(BRIEF) : ran('', 1)));
  await spawn($, { prompt: '/build 4270' });
  expect(seen.runs.map((r) => [r.argv[0], r.cwd])).toEqual([
    ['/work/target/debug/xtask', '/work'],
    ['cargo', '/work'],
  ]);
  expect(seen.spawned[0]?.prompt).toBe(BRIEF);
});

test('a marked prompt that cannot expand is refused by name, and nothing starts', async ($, on) => {
  const seen = spawning(on, () => ({ value: { exitCode: 1, stdout: '', stderr: 'xtask: no branch records #4270', isStdoutTruncated: false, isStderrTruncated: false } }));
  const none = await spawn($, { prompt: '/build 4270\nmore' });
  expect(none.deny).toMatch(/`\/build 4270` marks a builder, and `xtask ticket brief 4270` rendered no brief \(cargo .*no branch records #4270\)/);
  const malformed = await spawn($, { prompt: '/build 4270 4271' });
  expect(malformed.deny).toMatch(/`\/build 4270 4271` marks a builder, and is not `\/build <n>` alone/);
  expect((await spawn($, { prompt: '/build' })).deny).toMatch(/is not `\/build <n>` alone/);
  expect(seen.spawned.length).toBe(0);
});

test('a brief with no heading or no worktree line is refused', async ($, on) => {
  let text = 'a stray line\n' + BRIEF;
  const seen = spawning(on, () => ran(text));
  expect((await spawn($, { prompt: '/build 4270' })).deny).toMatch(/printed no `## Brief` line first/);
  text = '## Brief\n\nno worktree here\n';
  expect((await spawn($, { prompt: '/build 4270' })).deny).toMatch(/names no `Worktree:` line/);
  expect(seen.spawned.length).toBe(0);
});

test('a marked spawn the hook failed on is refused, not started without its brief', async ($, on) => {
  const seen = spawning(on, () => ran(BRIEF), () => {
    throw new Error('no root');
  });
  expect((await spawn($, { prompt: '/build 4270' })).deny).toMatch(/the agent.spawn hook failed .* before its brief was rendered/);
  expect((await spawn($, { prompt: 'Read README.md.' })).deny).toBe(undefined);
  expect(seen.spawned.map((s) => s.prompt)).toEqual(['Read README.md.']);
});

test('an unmarked prompt passes untouched, with no xtask run', async ($, on) => {
  const seen = spawning(on, () => ran(BRIEF));
  for (const prompt of ['Read README.md.', 'then /build 4270', ' /build 4270', '/builder 4270', '\n/build 4270']) {
    await spawn($, { prompt, subagentType: 'Explore' });
  }
  expect(seen.runs.length).toBe(0);
  expect(seen.spawned.map((s) => [s.prompt, s.subagentType, s.cwd])).toEqual([
    ['Read README.md.', 'Explore', undefined],
    ['then /build 4270', 'Explore', undefined],
    [' /build 4270', 'Explore', undefined],
    ['/builder 4270', 'Explore', undefined],
    ['\n/build 4270', 'Explore', undefined],
  ]);
});

test("a workflow agent's prompt alone is expanded, a fork is refused, a resumed run passes untouched", async ($, on) => {
  const seen = spawning(on, () => ran(BRIEF));
  const workflow = { runId: 'wf_1', agentIndex: 1 };
  await spawn($, { prompt: '/build 4270', subagentType: 'builder', workflow });
  const fork = await spawn($, { prompt: '/build 4270', fork: true });
  expect(fork.deny).toMatch(/a fork cannot be retyped to `builder`/);
  await spawn($, { prompt: '/build 4270', resume: { agentId: 'agent-0' } });
  expect(seen.spawned.map((s) => [s.prompt, s.subagentType, s.cwd])).toEqual([
    [BRIEF, 'builder', undefined],
    ['/build 4270', undefined, undefined],
  ]);
});

test("a subagent's call carries its own cwd to permit, which runs at the session root", async ($, on) => {
  const stdins: { cwd: string; agent_id?: string }[] = [];
  const runs: { argv0: string; cwd?: string }[] = [];
  on('session.id', () => ({ value: 'session' }));
  on('session.cwd', () => ({ value: '/work-worktrees/t4270' }));
  on('session.root', () => ({ value: '/work' }));
  on('fs.exists', (_$, e) => ({ value: e.path === '/work/target/debug/xtask' }));
  on('process.run', (_$, e) => {
    stdins.push(JSON.parse(String(e.init?.stdin)));
    runs.push({ argv0: e.argv[0] ?? '', cwd: e.init?.cwd });
    return ran('');
  });
  on('tool.call', () => ({ result: { stdout: '' } }));
  await $.tool.call({ tool: 'Bash', command: 'git status', agentId: 'agent-1' });
  expect(runs).toEqual([{ argv0: '/work/target/debug/xtask', cwd: '/work' }]);
  expect(stdins[0]?.cwd).toBe('/work-worktrees/t4270');
});
