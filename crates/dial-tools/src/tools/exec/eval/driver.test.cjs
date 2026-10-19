'use strict';
// Standalone transport regressions: make test-drivers
// Run with each supported Node release; no sandbox or desktop host is required.
const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

function wireSafe(value) {
  if (typeof value === 'string') assert.equal(value, value.toWellFormed());
  else if (value && typeof value === 'object') {
    for (const [key, item] of Object.entries(value)) { wireSafe(key); wireSafe(item); }
  }
}

function run(frames, python = false) {
  const child = spawnSync(python ? (process.env.DIAL_TEST_PYTHON || 'python3') : process.execPath,
    [path.join(__dirname, python ? '../kernel/driver.py' : 'driver.cjs')], {
      input: frames.map(frame => JSON.stringify(frame) + '\n').join(''),
      encoding: 'utf8', timeout: 10000, maxBuffer: 16 * 1024 * 1024,
      stdio: python ? ['pipe', 'pipe', 'pipe'] : ['pipe', 'pipe', 'pipe', 'pipe'],
    });
  assert.ifError(child.error);
  assert.equal(child.status, 0, child.stderr);
  const output = (python ? child.stdout : child.output[3]).trim().split('\n').map(JSON.parse);
  output.forEach(wireSafe);
  return output;
}

const cell = (id, code) => ({ id, code });
const reply = (callback_id, operation_id, data, text = '', is_error = false) =>
  ({ callback_id, operation_id, result: { data, text, is_error } });

test('real REPL starts, keeps lexical declarations/await, reports errors and recovers', () => {
  const output = run([
    cell(1, 'const saved = await Promise.resolve(40); let counter = 1; class Box {};'),
    cell(2, 'saved + ++counter'),
    cell(3, 'throw new Error("sync failure")'),
    cell(4, 'await Promise.reject(new Error("await failure"))'),
    cell(5, 'const ='),
    cell(6, 'saved + counter'),
    cell(7, 'new Box() instanceof Box'),
  ]);
  assert.equal(output.length, 7);
  assert.equal(output[0].result.error, null);
  assert.equal(output[1].result.value, '42');
  assert.match(output[2].result.error, /sync failure/);
  assert.match(output[3].result.error, /await failure/);
  assert.match(output[4].result.error, /SyntaxError/);
  assert.equal(output[5].result.value, '42');
  assert.equal(output[6].result.value, 'true');
});

test('all wire boundaries repair lone surrogates without damaging emoji or literal escapes', () => {
  const output = run([
    cell(1, 'console.log("x".repeat(19999) + "😀"); console.error("x".repeat(19999) + "😀")'),
    cell(2, '"x".repeat(19998) + "😀"'), // inspect adds an opening quote before truncation
    cell(3, 'throw { stack: "😀" + "x".repeat(19999) }'), // tail slice starts at low surrogate
    cell(4, 'tool.call("test", {"\\ud800": "\\udfff", emoji: "😀", literal: "\\\\ud800"})'),
    reply(1, 4, null),
    cell(5, '__dial_display__("x".repeat(19999) + "😀" + "\\ud800")'),
    reply(2, 5, null),
    cell(6, 'tool.call("denied")'),
    reply(3, 6, { nested: '\ud800' }, 'denied', true),
  ]);
  const results = output.filter(frame => frame.result).map(frame => frame.result);
  assert.match(results[0].stdout, /\ufffd/);
  assert.match(results[0].stderr, /\ufffd/);
  assert.match(results[1].value, /\ufffd/);
  assert.match(results[2].error, /^\ufffd/);
  const args = output.find(frame => frame.callback === 'tool').arguments;
  assert.deepEqual(args, { '\ufffd': '\ufffd', emoji: '😀', literal: '\\ud800' });
  assert.equal(results[5].tool_error.data.nested, '\ufffd');
  assert.match(output.find(frame => frame.callback === 'display').text, /😀\ufffd$/);
});

for (const python of [false, true]) {
  test(`actual ${python ? 'Python' : 'JS'} computer prelude consumes raw bridge values/text/images and propagates denial`, () => {
    const source = fs.readFileSync(path.join(__dirname, '../../../../../dial-computer/src/computer/js', python ? 'prelude.py' : 'prelude.js'), 'utf8');
    const windows = [{ id: '7', app: 'Editor', title: 'Document' }];
    const artifact = { data: 'aW1hZ2U=', encoding: 'base64', media_type: 'image/png' };
    const output = run([
      cell(1, source),
      cell(2, 'await computer.windows()'), reply(1, 2, { value: windows }),
      cell(3, 'await computer.capabilities()'), reply(2, 3, { backend: 'fixture', screenshot: true }),
      cell(4, 'await computer.run("display(42); 19")'),
      reply(3, 4, { value: 19, text: 'host display', screenshots: 1, artifact }),
      ...(!python ? [reply(4, 4, null)] : []),
      cell(5, 'await computer.window(7)'), reply(python ? 4 : 5, 5, { value: windows[0] }),
      cell(6, 'await computer.screenshot()'), reply(python ? 5 : 6, 6, { value: artifact, artifact }),
      cell(7, 'await computer.run("denied")'), reply(python ? 6 : 7, 7, null, 'denied by confirm', true),
      cell(8, 'await computer.run("0")'), reply(python ? 7 : 8, 8, { value: 0 }),
      cell(9, 'await computer.run("malformed")'), reply(python ? 8 : 9, 9, 'not an object'),
    ], python);
    const results = output.filter(frame => frame.result).map(frame => frame.result);
    assert.equal(results.length, 9);
    assert.equal(results[0].error, null);
    assert.match(results[1].value, /Document/);
    assert.match(results[2].value, /fixture/);
    assert.equal(results[3].value, '19');
    if (python) assert.match(results[3].stdout, /host display/);
    else assert.equal(output.find(frame => frame.callback === 'display').text, 'host display');
    assert.match(results[4].value, /7/);
    assert.match(results[5].value, /aW1hZ2U=/);
    assert.match(results[6].error, /denied by confirm/);
    assert.equal(results[7].value, '0');
    assert.match(results[8].error, /invalid response/);
    const calls = output.filter(frame => frame.callback === 'prelude');
    assert.ok(calls.every(frame => frame.name === 'computer'));
    assert.equal(calls[0].parameters.chain[0].method, 'windows');
  });
}

test('throwing falsy values is still a failed cell and leaves the REPL usable', () => {
  const output = run([
    cell(1, 'throw null'), cell(2, 'throw undefined'), cell(3, 'throw 0'),
    cell(4, 'throw ""'), cell(5, 'await Promise.reject(null)'), cell(6, 'await Promise.reject(undefined)'),
    cell(7, 'await Promise.reject(0)'), cell(8, '21 * 2'),
  ]);
  assert.deepEqual(output.slice(0, 7).map(frame => frame.result.error), ['null', 'undefined', '0', '', 'null', 'undefined', '0']);
  assert.equal(output[7].result.value, '42');
});
