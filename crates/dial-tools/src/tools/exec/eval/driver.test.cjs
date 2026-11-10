'use strict';
// Standalone transport regressions: make test-drivers
// Run with each supported Node release; no sandbox or desktop host is required.
const assert = require('node:assert/strict');
const { spawn, spawnSync } = require('node:child_process');
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

for (const python of [false, true]) {
  test(`portable binary helpers use explicit endian and reject truncation (${python ? 'Python' : 'Node'})`, () => {
    const install = (!python ? fs.readFileSync(path.join(__dirname, 'binary.js'), 'utf8') + '\n' : '') + fs.readFileSync(path.join(__dirname, python ? 'portable.py' : 'portable.js'), 'utf8');
    const code = python ? "dial.hex(dial.pack('>Ih', [0x01020304, -2]))" : "dial.hex(dial.pack('>Ih', [0x01020304, -2]))";
    const invalid = python ? "dial.pack('<B', [256])" : "dial.pack('<B', [256])";
    const truncated = python ? "dial.unpack('>I', bytes([1,2]))" : "dial.unpack('>I', [1,2])";
    const frames = run([cell(0, install),cell(1,code),cell(2,invalid),cell(3,truncated)],python);
    assert.match(frames[1].result.value, /01 02 03 04 ff fe/);
    assert.ok(frames[2].result.error);
    assert.ok(frames[3].result.error);
    assert.equal(frames.filter(frame => frame.callback).length, 0);
  });

  test(`portable JSON rejects nonfinite values before any write (${python ? 'Python' : 'Node'})`, () => {
    const install = fs.readFileSync(path.join(__dirname, python ? 'portable.py' : 'portable.js'), 'utf8');
    const code = python ? "dial.writeJson('bad.json', {'value':float('nan')})" : "await dial.writeJson('bad.json', {value:NaN})";
    const frames = run([cell(0, install), cell(1, code)], python);
    assert.equal(frames.filter(frame => frame.callback).length, 0);
    assert.ok(frames.find(frame => frame.id === 1).result.error);
  });

  test(`batch errors never replay possibly completed mutations (${python ? 'Python' : 'Node'})`, () => {
    const code = python ? "tool.batch([{'name':'write', 'args':{'path':'once'}}])" : "await tool.batch([{name:'write',args:{path:'once'}}])";
    const frames = run([cell(1, code), reply(1, 1, null, 'dial.batch failed after a write', true)], python);
    assert.equal(frames.filter(frame => frame.callback).length, 1);
    assert.match(frames.find(frame => frame.result).result.error, /failed after a write/);
  });

  test(`portable API reads bytes and gates dependent writes (${python ? 'Python' : 'Node'})`, () => {
    const install = fs.readFileSync(path.join(__dirname, python ? 'portable.py' : 'portable.js'), 'utf8');
    const read = python ? "list(dial.readBytes('source.bin'))" : "Array.from(await dial.readBytes('source.bin'))";
    const write = python ? "dial.writeBytes('new.bin', bytes([0, 255, 13, 10]))" : "await dial.writeBytes('new.bin', new Uint8Array([0,255,13,10]))";
    const batch = python ? "dial.batch([{'name':'read'}])\ndial.writeText('must-not-exist','bad')" : "await dial.batch([{name:'read'}]); await dial.writeText('must-not-exist','bad')";
    const frames = run([cell(0, install), cell(1, read), reply(1, 1, {content:'AP8NCg=='}),
      cell(2, write), reply(2, 2, {created:true}), cell(3, batch), reply(3, 3, [{is_error:true,text:'denied',data:null,status:'blocked'}])], python);
    const results = frames.filter(frame => frame.result);
    assert.equal(results[0].result.error, null);
    assert.match(results[1].result.value, /0, 255, 13, 10/);
    assert.match(results[3].result.error, /prerequisites failed/);
    const calls = frames.filter(frame => frame.callback);
    assert.equal(calls.length, 3, 'dependent write must never run');
    assert.deepEqual(calls[1].arguments, {path:'new.bin',content:'AP8NCg==',encoding:'base64'});
  });

  test(`portable search shrinks counterexamples and compare finds disagreements (${python ? 'Python' : 'Node'})`, () => {
    const install = fs.readFileSync(path.join(__dirname, python ? 'portable.py' : 'portable.js'), 'utf8');
    const sum = python
      ? "r = dial.search(lambda xs: sum(xs) < 50, dial.gen.array(dial.gen.int(0, 100)))\n[r['passed'], r['counterexample']]"
      : "const r = await dial.search(xs => xs.reduce((a, b) => a + b, 0) < 50, {gen: dial.gen.array(dial.gen.int(0, 100))}); JSON.stringify([r.passed, r.counterexample])";
    const differ = python
      ? "c = dial.compare(lambda x: abs(x), lambda x: x, dial.gen.int())\n[c['passed'], c['counterexample']]"
      : "const c = await dial.compare(x => Math.abs(x), x => x, {gen: dial.gen.int()}); JSON.stringify([c.passed, c.counterexample])";
    const agree = python
      ? "dial.compare(lambda s: s.upper().lower(), lambda s: s.lower(), dial.gen.string(alphabet='abC'), runs=50)['passed']"
      : "(await dial.compare(s => s.toUpperCase().toLowerCase(), s => s.toLowerCase(), {gen: dial.gen.string({alphabet: 'abC'}), runs: 50})).passed";
    const frames = run([cell(0, install), cell(1, sum), cell(2, differ), cell(3, agree)], python);
    const results = frames.filter(frame => frame.result).map(frame => frame.result);
    assert.equal(results[1].error, null);
    assert.match(results[1].value, python ? /\[False, \[50\]\]/ : /\[false,\[50\]\]/);
    assert.match(results[2].value, python ? /\[False, -1\]/ : /\[false,-1\]/);
    assert.match(results[3].value, python ? /True/ : /true/);
    assert.equal(frames.filter(frame => frame.callback).length, 0);
  });
}

test('Python shorthand preserves dictionary/keyword calls, validation and controller denial', () => {
  const parameters = { path: 'summary.csv', content: 'a,b\n' };
  const output = run([
    cell(1, 'tool.fixture({"path": "summary.csv", "content": "a,b\\n"})["data"]'),
    reply(1, 1, 42),
    cell(2, 'tool.fixture(path="summary.csv", content="a,b\\n")["data"]'),
    reply(2, 2, 42),
    cell(3, 'tool.fixture()["data"]'), reply(3, 3, 42),
    cell(4, 'tool.fixture({"path": "left"}, path="right")'),
    cell(5, 'tool.fixture(["not", "an", "object"])'),
    cell(6, 'tool.fixture({"path": "denied"})'),
    reply(4, 6, { reason: 'fixture' }, 'fixture denied', true),
    cell(7, '40 + 2'),
  ], true);
  const results = output.filter(frame => frame.result).map(frame => frame.result);
  assert.equal(results.length, 7);
  for (const result of [...results.slice(0, 3), results[6]]) {
    assert.equal(result.error, null);
    assert.equal(result.value, '42');
  }
  assert.match(results[3].error, /object or keyword arguments, not both/);
  assert.match(results[4].error, /arguments must be an object/);
  assert.match(results[5].error, /fixture denied/);
  assert.deepEqual(results[5].tool_error.data, { reason: 'fixture' });
  assert.deepEqual(output.filter(frame => frame.callback).map(frame => ({
    name: frame.name, arguments: frame.arguments, operation: frame.operation_id,
  })), [
    { name: 'fixture', arguments: parameters, operation: 1 },
    { name: 'fixture', arguments: parameters, operation: 2 },
    { name: 'fixture', arguments: {}, operation: 3 },
    { name: 'fixture', arguments: { path: 'denied' }, operation: 6 },
  ]);
});

test('callbacks wait for delayed replies after Node makes stdin nonblocking', { timeout: 5000 }, async t => {
  const child = spawn(process.execPath, [path.join(__dirname, 'driver.cjs')], {
    stdio: ['pipe', 'pipe', 'pipe', 'pipe'],
  });
  t.after(() => child.kill());
  const frames = [];
  let stderr = '';
  child.stderr.on('data', chunk => { stderr += chunk; });
  const lines = require('node:readline').createInterface({ input: child.stdio[3] });
  lines.on('line', line => {
    const frame = JSON.parse(line);
    frames.push(frame);
    if (frame.callback) setTimeout(() => child.stdin.end(JSON.stringify(reply(frame.callback_id, frame.operation_id, 42)) + '\n'), 50);
  });
  child.stdin.write(JSON.stringify(cell(1, "var input = (await import('node:process')).stdin; tool.call('fixture').data")) + '\n');
  const status = await new Promise((resolve, reject) => { child.once('error', reject); child.once('close', resolve); });
  assert.equal(status, 0, stderr);
  assert.equal(frames.length, 2);
  assert.equal(frames[1].result.error, null);
  assert.equal(frames[1].result.value, '42');
});

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

test('withheld globals and redeclarations explain the persistent namespace', () => {
  const output = run([
    cell(1, 'process.version'),
    cell(2, "const proc = await import('node:process'); typeof proc.version"),
    cell(3, 'const proc = 1'),
    cell(4, 'undefinedThing'),
  ]);
  assert.equal(output.length, 4);
  assert.match(output[0].result.error, /ReferenceError: process is not defined[\s\S]*await import\('node:process'\)/);
  assert.equal(output[1].result.value, "'string'");
  assert.match(output[2].result.error, /already been declared[\s\S]*Top-level declarations persist across cells/);
  assert.match(output[3].result.error, /undefinedThing is not defined/);
  assert.doesNotMatch(output[3].result.error, /\[dial\]/);
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

for (const python of [false, true]) {
  test(`${python ? 'Python' : 'JS'} stream retention counters disclose bounded loss`, () => {
    const code = python ? 'print("x" * 12500); print("x" * 12500)' : 'console.log("x".repeat(12500)); console.log("x".repeat(12500))';
    const result = run([cell(1, code)], python)[0].result;
    assert.equal(result.output_integrity.original_bytes, 25002);
    assert.equal(result.output_integrity.retained_bytes, 20000);
    assert.match(result.stdout, /truncated/);
  });
}

test('tool.batch sends one ordered batch callback and falls back to serial calls when refused', () => {
  const output = run([
    cell(1, "(await tool.batch([{ name: 'read', args: { path: 'a' } }, { name: 'grep' }])).map(r => r.data).join(',')"),
    reply(1, 1, [{ text: '', data: 1, is_error: false }, { text: '', data: 2, is_error: false }]),
    cell(2, "tool.batch([{ name: 'x', args: { n: 1 } }, { name: 'y' }]).map(r => r.is_error ? 'E' : r.data).join(',')"),
    reply(2, 2, { code: 'unsupported_batch', executed: false }, 'batch not supported', true),
    reply(3, 2, 'X'),
    reply(4, 2, null, 'denied', true),
    cell(3, "tool.batch([{ name: '' }])"),
    cell(4, 'tool.batch([])'),
  ]);
  const results = output.filter(frame => frame.result).map(frame => frame.result);
  assert.equal(results[0].value, "'1,2'");
  assert.equal(results[1].value, "'X,E'");
  assert.match(results[2].error, /entry 0: tool name must be a nonempty string/);
  assert.equal(results[3].value, '[]');
  assert.deepEqual(output.filter(frame => frame.callback).map(frame => ({ name: frame.name, arguments: frame.arguments })), [
    { name: 'dial.batch', arguments: { calls: [{ name: 'read', arguments: { path: 'a' } }, { name: 'grep', arguments: {} }] } },
    { name: 'dial.batch', arguments: { calls: [{ name: 'x', arguments: { n: 1 } }, { name: 'y', arguments: {} }] } },
    { name: 'x', arguments: { n: 1 } },
    { name: 'y', arguments: {} },
  ]);
});

test('Python tool.batch sends one ordered batch callback and falls back to serial calls when refused', () => {
  const output = run([
    cell(1, "','.join(str(r['data']) for r in tool.batch([{'name': 'read', 'args': {'path': 'a'}}, {'name': 'grep'}]))"),
    reply(1, 1, [{ text: '', data: 1, is_error: false }, { text: '', data: 2, is_error: false }]),
    cell(2, "','.join('E' if r['is_error'] else str(r['data']) for r in (await tool.batch([{'name': 'x', 'args': {'n': 1}}, {'name': 'y'}])))"),
    reply(2, 2, { code: 'unsupported_batch', executed: false }, 'batch not supported', true),
    reply(3, 2, 'X'),
    reply(4, 2, { reason: 'fixture' }, 'denied', true),
    cell(3, "tool.batch([{'name': ''}])"),
    cell(4, 'tool.batch([])'),
    cell(5, "tool.batch([{'name': 'a'}, {'name': 'b'}])"),
    reply(5, 5, [{ text: '', data: 1, is_error: false }]),
    cell(6, "tool.batch([{'name': 'a'}] * 129)"),
    cell(7, "tool.batch([{'name': 'a'}, {'name': 'b'}])"),
    reply(6, 7, null, 'cell callback limit exceeded', true),
  ], true);
  const results = output.filter(frame => frame.result).map(frame => frame.result);
  assert.equal(results[0].value, "'1,2'");
  assert.equal(results[1].value, "'X,E'");
  assert.match(results[2].error, /entry 0: tool name must be a nonempty string/);
  assert.equal(results[3].value, '[]');
  assert.match(results[4].error, /malformed tool.batch result/);
  assert.match(results[5].error, /at most 128/);
  assert.match(results[6].error, /cell callback limit exceeded/, 'only a refusal of the reserved name falls back');
  assert.deepEqual(output.filter(frame => frame.callback).map(frame => ({ name: frame.name, arguments: frame.arguments })), [
    { name: 'dial.batch', arguments: { calls: [{ name: 'read', arguments: { path: 'a' } }, { name: 'grep', arguments: {} }] } },
    { name: 'dial.batch', arguments: { calls: [{ name: 'x', arguments: { n: 1 } }, { name: 'y', arguments: {} }] } },
    { name: 'x', arguments: { n: 1 } },
    { name: 'y', arguments: {} },
    { name: 'dial.batch', arguments: { calls: [{ name: 'a', arguments: {} }, { name: 'b', arguments: {} }] } },
    { name: 'dial.batch', arguments: { calls: [{ name: 'a', arguments: {} }, { name: 'b', arguments: {} }] } },
  ]);
});

test('Python tool.batch entries count against the cell callback limit', () => {
  const frames = [cell(1, "r = tool.batch([{'name': 'a'}] * 128)\ntool.a()")];
  frames.push(reply(1, 1, Array.from({ length: 128 }, (_, n) => ({ text: '', data: n, is_error: false }))));
  const output = run(frames, true);
  const result = output.find(frame => frame.result).result;
  assert.match(result.error, /cell callback limit exceeded/);
  assert.equal(output.filter(frame => frame.callback).length, 1);
});
