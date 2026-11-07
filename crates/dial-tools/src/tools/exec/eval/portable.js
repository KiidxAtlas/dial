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
  return Object.freeze({
    hex: (...args) => __dial_binary__.hex(...args),
    pack: (...args) => __dial_binary__.pack(...args),
    unpack: (...args) => __dial_binary__.unpack(...args),
    read,
    readText: async path => (await read(path)).content,
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
