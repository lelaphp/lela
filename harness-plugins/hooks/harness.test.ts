// The function hook hands permit the person's answers (#2247) only where
// a denial an approval could lift asks for them, and only the engine's
// answered AskUserQuestion records; the engine beneath is this test's.
import { expect, test } from 'claude-code/testing';

const ANSWERED = { tool_use_id: 'toolu_ask', tool: 'AskUserQuestion', input: { q: 1 }, result: { answers: {} } };
const UNANSWERED = { tool_use_id: 'toolu_open', tool: 'AskUserQuestion', input: {} };
const DENY = '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"By a person. (harness.self)"}}';

test('a harness.self denial is asked again with the answers, and lifted', async ($, on) => {
  const stdins: Record<string, unknown>[] = [];
  const ran = (stdout: string) => ({
    value: { exitCode: 0, stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false },
  });
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
