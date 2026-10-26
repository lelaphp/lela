// The function hook hands permit the person's answers (#2247) only where
// a denial an approval could lift asks for them, and only the engine's
// answered AskUserQuestion records; the engine beneath is this test's. A
// denial is kept in $.store, and an Edit or a Write no judge saw is a deny,
// whether no candidate judged or the hook threw (#3489).
import type { On } from 'claude-code';
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
