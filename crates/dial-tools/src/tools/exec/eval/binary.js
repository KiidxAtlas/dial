// Bounded standard-library binary helpers; no filesystem or process authority.
var __dial_binary__ = (() => {
  const bytes = input => {
    if (!(input instanceof Uint8Array) && !Array.isArray(input)) throw new TypeError('expected bytes');
    if (Array.from(input).some(n => !Number.isInteger(n) || n < 0 || n > 255)) throw new RangeError('invalid byte');
    return new Uint8Array(input);
  };
  const fields = format => {
    if (typeof format !== 'string' || format.length > 256 || !/^[<>](?:[1-9][0-9]*|)[bBhHiIqQfdx]+/.test(format)) throw new TypeError('format needs explicit < or > byte order and bBhHiIqQfdx fields');
    const out = [];
    let end = 1;
    for (const match of format.slice(1).matchAll(/([1-9][0-9]*)?([bBhHiIqQfdx])/g)) {
      if (match.index + 1 !== end) throw new TypeError('invalid binary format');
      end += match[0].length;
      const count = Number(match[1] || 1);
      if (count + out.length > 256) throw new RangeError('at most 256 binary fields');
      for (let n = 0; n < count; n++) out.push(match[2]);
    }
    if (end !== format.length) throw new TypeError('invalid binary format');
    return out;
  };
  const width = code => ({b:1,B:1,h:2,H:2,i:4,I:4,q:8,Q:8,f:4,d:8,x:1})[code];
  const method = code => ({b:'Int8',B:'Uint8',h:'Int16',H:'Uint16',i:'Int32',I:'Uint32',q:'BigInt64',Q:'BigUint64',f:'Float32',d:'Float64'})[code];
  return Object.freeze({
    hex: (input, offset = 0, length = 256) => {
      const data = bytes(input);
      if (!Number.isSafeInteger(offset) || offset < 0 || offset > data.length || !Number.isSafeInteger(length) || length < 0 || length > 1024) throw new RangeError('hex offset must be in range; length is 0–1024');
      const rows = [];
      for (let pos = offset; pos < Math.min(data.length, offset + length); pos += 16) {
        const chunk = data.slice(pos, Math.min(pos + 16, offset + length));
        rows.push(pos.toString(16).padStart(8, '0') + '  ' + Array.from(chunk, n => n.toString(16).padStart(2, '0')).join(' ').padEnd(47, ' ') + '  |' + Array.from(chunk, n => n >= 32 && n < 127 ? String.fromCharCode(n) : '.').join('') + '|');
      }
      return rows.join('\n');
    },
    pack: (format, values) => {
      const spec = fields(format);
      if (!Array.isArray(values) || values.length !== spec.filter(code => code !== 'x').length) throw new RangeError('binary value count differs from format');
      const output = new Uint8Array(spec.reduce((n, code) => n + width(code), 0));
      const view = new DataView(output.buffer);
      let pos = 0, index = 0;
      for (const code of spec) {
        const size = width(code);
        if (code !== 'x') {
          let value = values[index++];
          if (code === 'q' || code === 'Q') {
            if (typeof value !== 'bigint' && !Number.isSafeInteger(value)) throw new RangeError('64-bit integers need bigint or safe integers');
            value = BigInt(value);
            const signed = code === 'q';
            if (value < (signed ? -(1n << 63n) : 0n) || value >= (1n << (signed ? 63n : 64n))) throw new RangeError('integer outside field range');
          } else if (code !== 'f' && code !== 'd') {
            const signed = code === code.toLowerCase();
            if (!Number.isSafeInteger(value) || value < (signed ? -(2 ** (size * 8 - 1)) : 0) || value >= 2 ** (size * 8 - (signed ? 1 : 0))) throw new RangeError('integer outside field range');
          } else if (typeof value !== 'number') throw new TypeError('float field needs a number');
          view['set' + method(code)](pos, value, format[0] === '<');
        }
        pos += size;
      }
      return output;
    },
    unpack: (format, input) => {
      const spec = fields(format), data = bytes(input);
      if (data.length !== spec.reduce((n, code) => n + width(code), 0)) throw new RangeError('byte length differs from format');
      const view = new DataView(data.buffer), out = [];
      let pos = 0;
      for (const code of spec) {
        if (code !== 'x') out.push(view['get' + method(code)](pos, format[0] === '<'));
        pos += width(code);
      }
      return out;
    },
  });
})();
