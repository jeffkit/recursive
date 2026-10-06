'use strict';
// Programmatic tool calling (PTC) bootstrap — issue #134.
//
// The host spawns a fresh Node process per `run_code` invocation and drives a
// newline-delimited JSON protocol. Nothing here is model-authored: this file
// is embedded in the binary (`include_str!`) and only the *program body* comes
// from the model.
//
//   host -> node : {"t":"program","source":"...","bindings":["Read", ...]}
//                  {"t":"call-result","id":N,"ok":bool,"value"|"error":...}
//   node -> host : {"t":"log","level":"log","text":"..."}
//                  {"t":"call","id":N,"tool":"Read","args":{...}}
//                  {"t":"result","value":"..."}
//                  {"t":"error","name":"...","message":"...","stack":"..."}
//                  (a completion value that cannot be serialized is reported
//                   as {"t":"error","name":"InvalidOutput",...} — the host
//                   classifies it as `invalid-output`, not as a thrown Error)
//
// stdout is the protocol channel (the program's own output is funnelled
// through `log` events), stdin carries the host's replies. `fs.writeSync` is
// used instead of `process.stdout.write` because it is synchronous and
// therefore ordered against the program's own execution.

const fs = require('fs');
const readline = require('readline');

// A pipe has a finite buffer: `fs.writeSync` writes what fits and then fails
// with EAGAIN, and a short write would merge the NEXT protocol message into
// the same line — corrupting every event that follows. Nap and retry until
// the whole message is on the wire (the host drains stdout as we write).
const writeAll = (text) => {
  const buf = Buffer.from(text, 'utf8');
  const nap = new Int32Array(new SharedArrayBuffer(4));
  let offset = 0;
  while (offset < buf.length) {
    try {
      offset += fs.writeSync(1, buf, offset);
    } catch (err) {
      if (err.code !== 'EAGAIN' && err.code !== 'EWOULDBLOCK') throw err;
      Atomics.wait(nap, 0, 0, 1);
    }
  }
};

const protocolWrite = (obj) => {
  writeAll(JSON.stringify(obj) + '\n');
};

// Strict form: throws when the value cannot be represented (a circular
// object). Used for the program's completion value, where a lossy fallback
// would silently hand the model a wrong answer.
const jsonStringifyStrict = (value) =>
  JSON.stringify(value, (_key, inner) =>
    typeof inner === 'bigint' ? inner.toString() : inner,
  );

// Lossy form for log arguments: a console.log of a circular object should
// still produce a line rather than fail the program.
const jsonStringifySafe = (value) => {
  try {
    return jsonStringifyStrict(value);
  } catch (_err) {
    return String(value);
  }
};

const formatArg = (value) => {
  if (typeof value === 'string') return value;
  if (value === undefined) return 'undefined';
  if (typeof value === 'bigint') return value.toString() + 'n';
  if (typeof value === 'symbol') return value.toString();
  if (typeof value === 'function') return '[Function]';
  return jsonStringifySafe(value);
};

// Console shim: exactly five methods, all funnelled into the ordered output
// ledger on the host side.
const consoleShim = {};
for (const level of ['debug', 'log', 'info', 'warn', 'error']) {
  consoleShim[level] = (...args) => {
    protocolWrite({ t: 'log', level, text: args.map(formatArg).join(' ') });
  };
}
globalThis.console = consoleShim;

// A direct `process.stdout.write` must not corrupt the protocol stream, so it
// is funnelled through the ledger too.
const toText = (chunk, encoding) => {
  if (typeof chunk === 'string') return chunk;
  if (Buffer.isBuffer(chunk)) return chunk.toString('utf8');
  if (chunk instanceof Uint8Array) return Buffer.from(chunk).toString('utf8');
  return String(chunk);
};
process.stdout.write = (chunk, encoding, callback) => {
  protocolWrite({ t: 'log', level: 'log', text: toText(chunk, encoding) });
  if (typeof encoding === 'function') encoding();
  else if (typeof callback === 'function') callback();
  return true;
};

const pending = new Map();
let nextId = 1;

let resolveProgram;
const programReady = new Promise((resolve) => {
  resolveProgram = resolve;
});

const rl = readline.createInterface({ input: process.stdin, terminal: false });
rl.on('line', (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch (_err) {
    return;
  }
  if (!msg || typeof msg !== 'object') return;
  if (msg.t === 'program') {
    resolveProgram(msg);
    return;
  }
  if (msg.t === 'call-result') {
    const waiter = pending.get(msg.id);
    if (!waiter) return;
    pending.delete(msg.id);
    if (msg.ok) waiter.resolve(msg.value);
    else waiter.reject(new Error(String(msg.error)));
  }
});

function makeBinding(tool) {
  return function binding(args) {
    const id = nextId++;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      protocolWrite({
        t: 'call',
        id,
        tool,
        args: args === undefined ? {} : args,
      });
    });
  };
}

programReady.then(async (msg) => {
  const bindings = Array.isArray(msg.bindings) ? msg.bindings : [];
  const source = String(msg.source === undefined ? '' : msg.source);
  let result;
  try {
    const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
    const runner = new AsyncFunction(...bindings, source);
    result = await runner(...bindings.map(makeBinding));
  } catch (err) {
    protocolWrite({
      t: 'error',
      name: err && err.name ? String(err.name) : 'Error',
      message: err && err.message !== undefined ? String(err.message) : String(err),
      stack: err && err.stack ? String(err.stack) : '',
    });
    process.exit(1);
    return;
  }
  if (result === undefined) {
    protocolWrite({ t: 'result', value: null });
  } else if (typeof result === 'string') {
    protocolWrite({ t: 'result', value: result });
  } else {
    let serialized;
    try {
      serialized = jsonStringifyStrict(result);
    } catch (err) {
      // No lossy fallback here: `String(value)` would turn a circular value
      // into "[object Object]" and the model would never learn the run
      // produced something it cannot read. The message is just the underlying
      // reason — the host's `invalid-output` description prefixes it.
      protocolWrite({
        t: 'error',
        name: 'InvalidOutput',
        message: String(err && err.message !== undefined ? err.message : err),
        stack: '',
      });
      process.exit(1);
      return;
    }
    // `undefined` (a function / symbol / …) keeps the lossy rendering — it is
    // a readable summary, not a serialization failure.
    protocolWrite({
      t: 'result',
      value: serialized === undefined ? formatArg(result) : serialized,
    });
  }
  process.exit(0);
});
