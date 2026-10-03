/**
 * Do the payloads the renderer builds still match the Rust structs they are sent to?
 *
 * The renderer and the Rust side share nothing but JSON. Tauri converts the *argument* name of a
 * command between camelCase and snake_case, but a struct passed as that argument is deserialised by
 * serde exactly as written: `body_text` on the wire, never `bodyText`. A mismatch is invisible to
 * `node --check`, to `cargo build` and to every Rust test, and it fails only at runtime, only on
 * the command that carries the struct. It has already happened once — `send_mail` was called with
 * `bodyText`/`inReplyTo` and the 发送 button could never succeed (found and fixed in M1a-7).
 *
 * So the keys are compared here, mechanically:
 *   - every builder in `src/payload.js` is called with an empty argument to read its key set, and
 *   - the matching `pub struct` is parsed out of the Rust source named in `CONTRACTS`, and
 *   - both must be equal as sets: a key the struct does not have, and a field the payload forgot,
 *     are both failures (a forgotten field is a serde "missing field" at runtime).
 *
 * `src/app.js` must build these payloads through the builders; the check below fails if it stops,
 * because otherwise this file would be verifying dead code.
 *
 * Usage: node tools/payload_keys.mjs      (exit 0 = in sync, 1 = drifted)
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

const CONTRACTS = [
  { builder: 'outgoingMail', struct: 'OutgoingMail', file: 'crates/mail-core/src/model.rs' },
  { builder: 'draft', struct: 'Draft', file: 'crates/mail-core/src/userdb.rs' },
];

/** Load the renderer's builders without a browser: the file assigns to `globalThis`. */
function loadBuilders() {
  const source = readFileSync(join(root, 'src/payload.js'), 'utf8');
  const sandbox = {};
  vm.createContext(sandbox);
  vm.runInContext(source, sandbox, { filename: 'src/payload.js' });

  const builders = sandbox.__easyEmailPayload;
  if (!builders) {
    throw new Error('src/payload.js did not define globalThis.__easyEmailPayload (did it stop assigning to `global`?)');
  }
  return builders;
}

/** The field names of one `pub struct` in a Rust source file. */
function structFields(file, struct) {
  const source = readFileSync(join(root, file), 'utf8');
  const start = source.indexOf(`pub struct ${struct} {`);
  if (start < 0) {
    throw new Error(`${file}: no \`pub struct ${struct} {\` — the contract has no Rust side left`);
  }
  const end = source.indexOf('\n}', start);
  if (end < 0) {
    throw new Error(`${file}: could not find the end of \`pub struct ${struct}\``);
  }

  return new Set(
    [...source.slice(start, end).matchAll(/^\s*pub ([a-z_0-9]+)\s*:/gm)].map((match) => match[1])
  );
}

const problems = [];
const builders = loadBuilders();
const app = readFileSync(join(root, 'src/app.js'), 'utf8');

// The gate is only worth anything if the shipped code path uses the builders.
if (!app.includes('__easyEmailPayload')) {
  problems.push('src/app.js: never binds window.__easyEmailPayload, so nothing sends these shapes');
}

for (const { builder, struct, file } of CONTRACTS) {
  const before = problems.length;

  if (typeof builders[builder] !== 'function') {
    problems.push(`src/payload.js: ${builder}() is gone, but ${struct} is still a command argument`);
    continue;
  }
  if (!app.includes(`.${builder}(`)) {
    problems.push(`src/app.js: never calls ${builder}(), so the ${struct} contract is unverified`);
  }

  const sent = new Set(Object.keys(builders[builder]({})));
  const expected = structFields(file, struct);

  for (const key of sent) {
    if (!expected.has(key)) {
      problems.push(`${file}: ${struct} has no field \`${key}\`, but the payload sends it (serde would ignore it silently)`);
    }
  }
  for (const field of expected) {
    if (!sent.has(field)) {
      problems.push(
        `src/payload.js: ${builder}() omits \`${field}\` — ${struct} would fail to deserialise with "missing field"`
      );
    }
  }

  if (problems.length === before) {
    console.log(`${builder}() <-> ${struct}: ${sent.size} field(s) match`);
  }
}

if (problems.length > 0) {
  for (const problem of problems) console.error(`- ${problem}`);
  console.error('\npayload/struct drift: the renderer and serde disagree about the wire shape');
  process.exit(1);
}
