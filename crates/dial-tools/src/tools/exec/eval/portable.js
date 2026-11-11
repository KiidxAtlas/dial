// Portable convenience layer: every I/O operation still uses ordinary admitted tools.
var dial = (() => {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  const encode = bytes => {
    if (!(bytes instanceof Uint8Array) && !Array.isArray(bytes)) throw new TypeError('writeBytes expects Uint8Array or byte array');
    let out = '';
    for (let i = 0; i < bytes.length; i += 3) {
      const part = Array.from(bytes.slice(i, i + 3));
      if (part.some(n => !Number.isInteger(n) || n < 0 || n > 255)) throw new TypeError('invalid byte');
      const n = (part[0] << 16) | ((part[1] || 0) << 8) | (part[2] || 0);
      out += alphabet[n >>> 18] + alphabet[(n >>> 12) & 63] + (part.length > 1 ? alphabet[(n >>> 6) & 63] : '=') + (part.length > 2 ? alphabet[n & 63] : '=');
    }
    return out;
  };
  const decode = text => {
    const out = [];
    for (let i = 0; i < text.length; i += 4) {
      const n = (alphabet.indexOf(text[i]) << 18) | (alphabet.indexOf(text[i + 1]) << 12) | (Math.max(0, alphabet.indexOf(text[i + 2])) << 6) | Math.max(0, alphabet.indexOf(text[i + 3]));
      out.push((n >>> 16) & 255);
      if (text[i + 2] !== '=') out.push((n >>> 8) & 255);
      if (text[i + 3] !== '=') out.push(n & 255);
    }
    return new Uint8Array(out);
  };
  const read = async (path, options = {}) => (await tool.read({path, format:options.encoding || 'text', retain:options.retain === true})).data;
  const batch = async (entries, options = {}) => {
    if (options.collect !== undefined && typeof options.collect !== 'boolean') throw new TypeError('collect must be a boolean');
    const results = await tool.batch(entries);
    if (!options.collect && results.some(result => result.is_error || result.effects === 'unknown')) {
      const error = new Error('Batch prerequisites failed; completed effects remain. Inspect error.results before retrying.');
      error.results = results;
      error.result = {is_error:true, text:error.message, data:{results}};
      throw error;
    }
    return results;
  };
  // Property search: edge cases first, then seeded random inputs that grow with the run index;
  // a failing input is shrunk toward a minimal one. Deterministic for a given seed.
  const random = seed => {
    let state = seed >>> 0;
    return () => {
      state = (state + 0x6D2B79F5) >>> 0;
      let t = Math.imul(state ^ (state >>> 15), state | 1);
      t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  };
  const unique = (values, current) => {
    const seen = new Set([JSON.stringify(current)]);
    return values.filter(value => { const key = JSON.stringify(value); return !seen.has(key) && seen.add(key); });
  };
  const make = (draw, shrink = () => [], edges = []) => Object.freeze({
    draw, shrink, edges,
    map: f => make((r, size) => f(draw(r, size)), () => [], edges.map(f)),
    filter: keep => make((r, size) => { for (let i = 0; i < 100; i++) { const v = draw(r, size); if (keep(v)) return v; } throw new Error('gen.filter rejected 100 draws'); }, v => shrink(v).filter(keep), edges.filter(keep)),
  });
  const int = (lo = -1000, hi = 1000) => {
    if (!Number.isSafeInteger(lo) || !Number.isSafeInteger(hi) || lo > hi) throw new TypeError('gen.int expects safe integers lo <= hi');
    const target = Math.min(Math.max(0, lo), hi);
    const within = v => v >= lo && v <= hi;
    return make((r, size) => {
      const span = Math.min(hi - lo, Math.max(1, size * size));
      const from = Math.min(Math.max(target - Math.floor(span / 2), lo), hi - span);
      return r() < 0.8 ? from + Math.floor(r() * (span + 1)) : lo + Math.floor(r() * (hi - lo + 1));
    }, v => unique([target, v - Math.trunc((v - target) / 2), v - Math.sign(v - target)].filter(within), v),
    unique([0, 1, -1, lo, hi, lo + 1, hi - 1].filter(within), NaN));
  };
  const float = (lo = -1e6, hi = 1e6) => make(
    (r, size) => r() < 0.5 ? (r() * 2 - 1) * size : lo + r() * (hi - lo),
    v => unique([0, Math.trunc(v), v / 2].filter(x => x >= lo && x <= hi), v),
    unique([0, -0, 0.5, -0.5, lo, hi, Number.EPSILON].filter(x => x >= lo && x <= hi), NaN));
  const sequence = (item, min, max, join) => make(
    (r, size) => { const n = min + Math.floor(r() * (Math.min(max, min + size) - min + 1)); return join(Array.from({length:n}, () => item.draw(r, size))); },
    null, []);
  const array = (item, {min = 0, max = 20} = {}) => {
    const base = sequence(item, min, max, xs => xs);
    const shrink = xs => {
      const out = [];
      if (xs.length > min) {
        out.push(xs.slice(0, Math.max(min, Math.floor(xs.length / 2))), xs.slice(xs.length - Math.max(min, Math.floor(xs.length / 2))));
        for (let i = 0; i < xs.length && i < 32; i++) out.push([...xs.slice(0, i), ...xs.slice(i + 1)]);
      }
      for (let i = 0; i < xs.length && i < 32; i++) for (const smaller of item.shrink(xs[i]).slice(0, 4)) out.push([...xs.slice(0, i), smaller, ...xs.slice(i + 1)]);
      return unique(out, xs);
    };
    const edges = [...(min === 0 ? [[]] : []), ...item.edges.slice(0, 6).map(e => Array(Math.max(1, min)).fill(e))].filter(xs => xs.length <= max);
    return make(base.draw, shrink, edges);
  };
  const printable = Array.from({length:95}, (_, i) => String.fromCharCode(32 + i)).join('');
  const string = ({alphabet = printable, min = 0, max = 20} = {}) => {
    const chars = Array.from(alphabet);
    if (!chars.length) throw new TypeError('gen.string needs a nonempty alphabet');
    const ch = make(r => chars[Math.floor(r() * chars.length)], c => c === chars[0] ? [] : [chars[0]], chars.slice(0, 3));
    const list = array(ch, {min, max});
    return make((r, size) => list.draw(r, size).join(''), s => list.shrink(Array.from(s)).map(xs => xs.join('')),
      unique([...(min === 0 ? [''] : []), chars[0].repeat(Math.max(1, min)), ...chars.slice(0, 3).map(c => c.repeat(Math.max(1, min)))].filter(s => s.length <= max), null));
  };
  const tuple = (...items) => make(
    (r, size) => items.map(item => item.draw(r, size)),
    xs => unique(items.flatMap((item, i) => item.shrink(xs[i]).slice(0, 8).map(v => xs.map((x, j) => j === i ? v : x))), xs),
    items.every(item => item.edges.length) ? [items.map(item => item.edges[0]), ...items[0].edges.slice(1).map(e => [e, ...items.slice(1).map(item => item.edges[0])])] : []);
  const record = shape => {
    const keys = Object.keys(shape), inner = tuple(...keys.map(key => shape[key]));
    const build = values => Object.fromEntries(keys.map((key, i) => [key, values[i]]));
    return make((r, size) => build(inner.draw(r, size)), o => inner.shrink(keys.map(key => o[key])).map(build), inner.edges.map(build));
  };
  const pick = values => {
    if (!Array.isArray(values) || !values.length) throw new TypeError('gen.pick expects a nonempty array');
    return make(r => values[Math.floor(r() * values.length)], v => values.slice(0, Math.max(0, values.indexOf(v))).slice(0, 4), [values[0]]);
  };
  const oneOf = (...items) => make((r, size) => items[Math.floor(r() * items.length)].draw(r, size), () => [], items.flatMap(item => item.edges.slice(0, 3)));
  const gen = Object.freeze({int, float, string, array, tuple, record, pick, oneOf, bool: () => pick([false, true]), constant: v => make(() => v, () => [], [v])});
  const show = value => { try { const text = typeof value === 'string' ? JSON.stringify(value) : JSON.stringify(value, (_k, v) => typeof v === 'bigint' ? v + 'n' : v === undefined ? '<undefined>' : v); return text === undefined ? String(value) : text.length > 400 ? text.slice(0, 400) + '…' : text; } catch { return String(value); } };
  const verdict = async (property, input) => {
    try {
      const result = await property(input);
      return result === false ? 'property returned false' : null;
    } catch (error) {
      return error instanceof Error ? `${error.name}: ${error.message}` : 'threw ' + show(error);
    }
  };
  const search = async (property, options = {}) => {
    const {gen:input, runs = 200, seed = 0x5eed, cases = [], timeoutMs = 10000, shrinks = 400} = options;
    if (typeof property !== 'function' || !input || typeof input.draw !== 'function') throw new TypeError('dial.search(property, {gen, runs, seed, cases, timeoutMs}) needs a property function and a dial.gen generator');
    const r = random(seed), deadline = Date.now() + timeoutMs;
    const inputs = function* () {
      yield* cases;
      yield* input.edges;
      for (let i = 0; i < runs; i++) yield input.draw(r, 1 + Math.floor(i * 100 / Math.max(1, runs)));
    };
    let tried = 0;
    for (const candidate of inputs()) {
      if (Date.now() > deadline) return {passed:true, runs:tried, seed, stopped:'timeout', note:'No counterexample within the time budget; this is evidence, not proof.'};
      tried++;
      const failure = await verdict(property, candidate);
      if (failure === null) continue;
      let smallest = candidate, reason = failure, steps = 0;
      for (let improved = true; improved && steps < shrinks && Date.now() <= deadline;) {
        improved = false;
        for (const smaller of input.shrink(smallest) || []) {
          if (++steps > shrinks) break;
          const why = await verdict(property, smaller);
          if (why !== null) { smallest = smaller; reason = why; improved = true; break; }
        }
      }
      return {passed:false, runs:tried, seed, counterexample:smallest, failure:reason, original:candidate, shrinkSteps:steps, shown:show(smallest)};
    }
    return {passed:true, runs:tried, seed, note:'No counterexample found; this is evidence, not proof.'};
  };
  const outcome = async (f, x) => { try { return {value: await f(x)}; } catch (error) { return {threw: error instanceof Error ? error.name : typeof error}; } };
  const compare = async (left, right, options = {}) => {
    const {equal = (a, b) => show(a) === show(b), ...rest} = options;
    const result = await search(async x => {
      const [a, b] = [await outcome(left, x), await outcome(right, x)];
      const same = 'threw' in a || 'threw' in b ? ('threw' in a) === ('threw' in b) : equal(a.value, b.value);
      if (!same) throw new Error(`left ${'threw' in a ? 'threw ' + a.threw : show(a.value)} vs right ${'threw' in b ? 'threw ' + b.threw : show(b.value)}`);
    }, rest);
    return result;
  };
  return Object.freeze({
    hex: (...args) => __dial_binary__.hex(...args),
    pack: (...args) => __dial_binary__.pack(...args),
    unpack: (...args) => __dial_binary__.unpack(...args),
    read,
    readText: async path => (await read(path)).content,
    readLines: async (path, offset = 1, limit) => (await tool.read({path, format:'text', offset, ...(limit === undefined ? {} : {limit})})).data,
    lines: async function* (path, offset = 1) {
      while (offset !== null && offset !== undefined) {
        const page = (await tool.read({path, format:'text', offset})).data;
        const text = page.content.replace(/\r?\n$/, '');
        if (page.lines) yield* text.split(/\r?\n/);
        offset = page.next_offset;
      }
    },
    readJson: async path => JSON.parse((await read(path)).content),
    readBytes: async path => decode((await read(path, {encoding:'base64'})).content),
    writeText: async (path, content) => tool.write({path, content}),
    writeJson: async (path, value) => {
      const content = JSON.stringify(value, (_key, item) => {
        if (typeof item === 'number' && !Number.isFinite(item)) throw new TypeError('JSON requires finite numbers');
        return item;
      }, 2);
      if (typeof content !== 'string') throw new TypeError('value has no JSON representation');
      return tool.write({path, content:content + '\n'});
    },
    writeBytes: async (path, bytes) => tool.write({path, content:encode(bytes), encoding:'base64'}),
    run: async (command, options = {}) => tool.bash({...options, command}),
    batch,
    check: async (name, passed) => __dial_check__(name, passed),
    gen,
    search,
    compare,
    fresh: async receipt => (await read(receipt.path, {encoding:'base64'})).sha256 === receipt.sha256,
    restore: async (receipt, path = receipt.path) => {
      const original = await read(receipt.artifact, {encoding:'base64'});
      if (original.sha256 !== receipt.sha256) throw new Error('Retained bytes do not match the source receipt');
      return tool.write({path, content:original.content, encoding:'base64'});
    },
    snapshot: async paths => {
      if (!Array.isArray(paths) || !paths.length || paths.length > 16) throw new TypeError('snapshot expects 1–16 local paths');
      const reads = await batch(paths.map(path => ({name:'read',args:{path,format:'base64',retain:true}})));
      const manifest = reads.map(({data:{content, ...receipt}}) => receipt);
      for (const receipt of manifest) {
        if ((await read(receipt.path, {encoding:'base64'})).sha256 !== receipt.sha256) throw new Error('Snapshot source changed: ' + receipt.path + '; retained artifacts remain available.');
      }
      return {files:manifest, consistency:'Individual byte captures rechecked; not an atomic live-database snapshot.'};
    },
  });
})();
