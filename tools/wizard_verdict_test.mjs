/**
 * Does the wizard reach the right verdict for every way a connection test can come back?
 *
 *     node tools/wizard_verdict_test.mjs
 *
 * The order of the checks in `src/wizard.js` is the decision, so the cases that matter are the ones
 * where two checks overlap: an OAuth account whose IMAP was rejected *and* whose SMTP was rejected
 * must be told its address is wrong (the more serious answer), and a password account must never be
 * waved through as "offline" just because the network was silent. Both are asserted here, and
 * `tools/check.ps1` runs this file so they stay asserted.
 *
 * Loaded through `vm` exactly the way `tools/thread_keys_test.mjs` loads `src/threads.js`: the
 * renderer's scripts assign to `globalThis` so a node test can read them without a bundler.
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

const sandbox = {};
vm.createContext(sandbox);
vm.runInContext(readFileSync(join(root, 'src/wizard.js'), 'utf8'), sandbox, {
  filename: 'src/wizard.js',
});

const { verificationVerdict } = sandbox.__easyEmailWizard ?? {};
if (typeof verificationVerdict !== 'function') {
  console.error('- src/wizard.js did not define globalThis.__easyEmailWizard.verificationVerdict');
  process.exit(1);
}

const failures = [];
const check = (label, actual, expected) => {
  if (actual !== expected) failures.push(`${label}: got ${actual}, expected ${expected}`);
};
const checkNot = (label, actual, unwanted) => {
  if (actual === unwanted) failures.push(`${label}: got ${actual}, which is exactly what must not happen`);
};

/** A rejected OAuth account that otherwise looks like a fresh attempt. */
const base = {
  oauthAuthorized: true,
  imapOk: false,
  smtpOk: false,
  imapAuthRejected: false,
  smtpAuthRejected: false,
};

// ---------------------------------------------------------------- both legs fine
check(
  'both legs fine',
  verificationVerdict({ ...base, imapOk: true, smtpOk: true }),
  'accept'
);
// Even a password account is accepted when the servers said yes to both legs: the verdict is about
// the *failure* paths, and a working connection is one.
check(
  'both legs fine on a password account',
  verificationVerdict({
    ...base,
    oauthAuthorized: false,
    imapOk: true,
    smtpOk: true,
  }),
  'accept'
);

// ---------------------------------------------------------------- password accounts never ride on silence
check(
  'a password account that failed is retried, not saved',
  verificationVerdict({ ...base, oauthAuthorized: false }),
  'retry'
);
checkNot(
  'a password account is not waved through as offline',
  verificationVerdict({ ...base, oauthAuthorized: false }),
  'accept-offline'
);
// The order assertion behind the line above: SMTP silence must not turn a password failure into a
// saved account either.
check(
  'a password account with a silent SMTP leg still retries',
  verificationVerdict({ ...base, oauthAuthorized: false, imapOk: true }),
  'retry'
);
check(
  'a password account whose SMTP was rejected still retries',
  verificationVerdict({
    ...base,
    oauthAuthorized: false,
    imapOk: true,
    smtpAuthRejected: true,
  }),
  'retry'
);

// ---------------------------------------------------------------- the OAuth outcomes
check(
  'an OAuth account whose address does not match is rejected',
  verificationVerdict({ ...base, imapAuthRejected: true }),
  'reject-address'
);
check(
  'receiving works and sending is refused',
  verificationVerdict({ ...base, imapOk: true, smtpAuthRejected: true }),
  'accept-receive-only'
);
check(
  'nothing answered: the authorization is real, so save it',
  verificationVerdict({ ...base }),
  'accept-offline'
);

// ---------------------------------------------------------------- the order assertions
// IMAP being rejected outranks SMTP being rejected: an address that is not the account that
// authorized can never work, however the sending leg behaves.
check(
  'IMAP rejected wins over SMTP rejected',
  verificationVerdict({ ...base, imapAuthRejected: true, smtpAuthRejected: true }),
  'reject-address'
);
// And a leg that answered "no" is never "offline": the whole point of keeping the two apart.
checkNot(
  'a rejected IMAP leg is not called silence',
  verificationVerdict({ ...base, imapAuthRejected: true }),
  'accept-offline'
);
// A missing field must not throw: the caller passes values straight out of a report that may be
// from an older build (`#[serde(default)]` on the Rust side yields `false`).
check(
  'a report missing the new flags is handled',
  verificationVerdict({ oauthAuthorized: true, imapOk: true, smtpOk: false }),
  'accept-offline'
);

// **V2-D / C8.** The receive-only flag opens the one hole in "an account must be complete", so the
// hole has to be exactly as wide as the user's statement and no wider.
check(
  'a stated absence of SMTP saves the account',
  verificationVerdict({ ...base, imapOk: true, smtpOk: false, smtpNone: true }),
  'accept-receive-only'
);
// The guard that makes the flag safe: the same failed probe, without the statement, is still refused.
// Without this the checkbox would become "save anyway" for every typo. Both halves matter: a
// password account says "retry", and an already-authorized OAuth account keeps its own outcome
// rather than being handed a free pass by a box nobody ticked.
check(
  'an unflagged SMTP failure is still refused (password account)',
  verificationVerdict({
    oauthAuthorized: false,
    imapOk: true,
    smtpOk: false,
    smtpNone: false,
  }),
  'retry'
);
check(
  'an unflagged SMTP failure is not rescued for an OAuth account either',
  verificationVerdict({
    oauthAuthorized: true,
    imapOk: true,
    smtpOk: false,
    imapAuthRejected: false,
    smtpAuthRejected: true,
    smtpNone: false,
  }),
  'accept-receive-only'
);
// The flag is not a way out of a *credential* failure. With IMAP answering and refusing, the
// password account path must still say "retry" — and the flag must not appear anywhere in that
// sentence. (An OAuth account whose IMAP was refused keeps its own outcome, `reject-address`: the
// statement about SMTP says nothing about whether the credential works.)
check(
  'a rejected IMAP leg is not rescued by the SMTP flag',
  verificationVerdict({
    oauthAuthorized: false,
    imapOk: false,
    smtpOk: false,
    imapAuthRejected: true,
    smtpNone: true,
  }),
  'retry'
);
check(
  'the OAuth address mismatch still outranks the SMTP flag',
  verificationVerdict({
    oauthAuthorized: true,
    imapOk: false,
    smtpOk: false,
    imapAuthRejected: true,
    smtpNone: true,
  }),
  'reject-address'
);
// A missing flag behaves as false: every account written before this field existed.
check(
  'the flag defaults to off',
  verificationVerdict({ oauthAuthorized: true, imapOk: true, smtpOk: false }),
  'accept-offline'
);

if (failures.length > 0) {
  for (const failure of failures) console.error(`- ${failure}`);
  console.error('\nthe wizard would save or refuse an account for the wrong reason');
  process.exit(1);
}

console.log(
  `wizard verdicts: ${[
    'both legs fine accepted',
    'password failures retried',
    'a mismatched OAuth address refused',
    'receive-only distinguished from offline',
    'rejections outrank silence',
    'the receive-only flag is exactly as wide as the statement',
  ].join(' / ')}`
);
