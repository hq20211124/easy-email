/* Easy Email — what a failed connection test means.
 *
 * The wizard saves an account only after probing both endpoints (`verify_account` in
 * `crates/mail-core/src/lib.rs`), and the interesting part is what to do when a probe fails. That
 * decision used to live inline in `openAccountWizard`'s `onConfirm` closure, where three separate
 * bugs were fixed one at a time (M1a-69, M1a-82, and the Microsoft 365 case) and where nothing could
 * test it — the only way to check the order of the checks was to click through the dialog by hand.
 *
 * So it is a pure function of two booleans per leg, and `tools/wizard_verdict_test.mjs` pins it. The
 * **order of the checks is the decision**, which is why they are spelled out rather than written as
 * a lookup table: every one of the five outcomes below is a real failure somebody hit.
 */
(function (global) {
  'use strict';

  /**
   * What to do about a failed connection test.
   *
   * Five outcomes, and the order of the checks *is* the decision:
   *
   * 1. both legs fine                        → 'accept'
   * 2. a password account that failed        → 'retry'    (usually a wrong code; keep the dialog open)
   * 3. OAuth, IMAP rejected                  → 'reject-address'  (the address is not the account that
   *                                            authorized; saving it creates an account that can never
   *                                            work — M1a-82)
   * 4. OAuth, IMAP fine but SMTP rejected    → 'accept-receive-only' (receiving works; sending is a
   *                                            server-side policy the user must fix — M365, design/01 P2b)
   * 5. OAuth, nothing answered               → 'accept-offline'    (the authorization is real and stored;
   *                                            silence is usually this network — M1a-69)
   *
   * `oauthAuthorized` is what keeps an ordinary password account from being waved through as
   * "offline": without a stored authorization there is nothing to save, and a typo in the password
   * would be recorded as a working account.
   *
   * 6. the user said this server has no SMTP → 'accept-receive-only'  (a receive-only mail store is
   *    not a broken account, and refusing to save it would be the protection misfiring —
   *    `design/20-v0.2-plan.md` C8. The flag has to come from the user: without it a mistyped host
   *    still yields no account.)
   *
   * `smtpNone` is checked before the `oauthAuthorized` gate and only for password accounts, because
   * that combination is the one with something worth saving: a working IMAP login plus an explicit
   * statement that there is nothing to send to.
   */
  function verificationVerdict({
    oauthAuthorized,
    imapOk,
    smtpOk,
    imapAuthRejected,
    smtpAuthRejected,
    smtpNone = false,
  }) {
    if (imapOk && smtpOk) return 'accept';
    // A stated absence is not a failure to retry: the account is exactly as the user described it.
    if (imapOk && smtpNone) return 'accept-receive-only';
    if (!oauthAuthorized) return 'retry';
    if (imapAuthRejected) return 'reject-address';
    if (imapOk && smtpAuthRejected) return 'accept-receive-only';
    return 'accept-offline';
  }

  global.__easyEmailWizard = { verificationVerdict };
})(typeof window === 'undefined' ? globalThis : window);
