// The desk registers no hook yet (#3490): a call passes through it to the
// engine beneath untouched, which is this test's.
import { expect, test } from 'claude-code/testing';

test('a call passes through the desk untouched', async ($, on) => {
  on('tool.call', () => ({ result: 'ran' }));
  expect((await $.tool.call({ tool: 'Bash', command: 'ls' })).result).toBe('ran');
});
