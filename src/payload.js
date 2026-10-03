/* Easy Email — the payload shapes sent to the commands that take a **struct**.
 *
 * Two commands do: `send_mail` takes `mail: OutgoingMail`, `save_draft` takes `draft: Draft`.
 * Tauri translates a command *argument* name between camelCase and snake_case, but it does not
 * touch anything inside the value — the struct is deserialised by serde, so its fields go over the
 * wire under their Rust names, exactly. A camelCase field is neither a compile error nor a failing
 * test on either side of the boundary; it is a runtime `missing field \`body_text\`` raised on the
 * click that matters, while every other payload of the same command keeps working.
 *
 * That is not hypothetical: `send_mail` was called with `bodyText`/`inReplyTo` from v0.1 and the
 * 发送 button could therefore never succeed. The shapes live here, in one place, and
 * `tools/payload_keys.mjs` (run by `tools/check.ps1`) compares these keys against the Rust structs
 * they feed, so the two cannot drift apart again without a red gate.
 *
 * The frontend reads the other direction (`detail.from_addr`, `m.message_id`) in snake_case for the
 * same reason: that is the shape serde writes.
 */
(function (global) {
  'use strict';

  /** `OutgoingMail` — crates/mail-core/src/model.rs. */
  function outgoingMail(fields) {
    return {
      to: fields.to ?? [],
      cc: fields.cc ?? [],
      bcc: fields.bcc ?? [],
      subject: fields.subject ?? '',
      body_text: fields.bodyText ?? '',
      body_html: fields.bodyHtml ?? null,
      in_reply_to: fields.inReplyTo ?? null,
      references: fields.references ?? [],
      attachments: fields.attachments ?? [],
      // Left null by the composer: the outbox fixes a Message-ID at enqueue time so the message keeps
      // one identity across a retry, and a one-shot send gets one generated server-side.
      message_id: fields.messageId ?? null,
    };
  }

  /**
   * `OutgoingAttachment` — crates/mail-core/src/model.rs.
   *
   * `sha256` is the identity of the file as the user chose it: it is what lets the send path notice
   * that the file was edited or deleted in between, instead of delivering something they never
   * approved (design/07 §5.5). The picker fills it in; a draft saved before it existed has `null`
   * and the backend completes it at send time.
   */
  function attachment(fields) {
    return {
      name: fields.name ?? '',
      mime: fields.mime ?? 'application/octet-stream',
      path: fields.path ?? '',
      size: fields.size ?? 0,
      sha256: fields.sha256 ?? null,
    };
  }

  /** `Draft` — crates/mail-core/src/userdb.rs. */
  function draft(fields) {
    return {
      id: fields.id,
      account_id: fields.accountId,
      to_addrs: fields.toAddrs ?? '',
      cc_addrs: fields.ccAddrs ?? '',
      bcc_addrs: fields.bccAddrs ?? '',
      subject: fields.subject ?? '',
      body_text: fields.bodyText ?? '',
      body_html: fields.bodyHtml ?? null,
      attachments_json: fields.attachmentsJson ?? '[]',
      in_reply_to: fields.inReplyTo ?? null,
      references: fields.referencesJson ?? '[]',
      identity: fields.identity ?? null,
      created_at: fields.createdAt,
      updated_at: fields.updatedAt,
    };
  }

  /**
   * The composer's attachment list <-> the JSON kept in `drafts.attachments_json`.
   *
   * Paths and metadata only, never bytes (design/07 §3): a draft carrying a 100 MB attachment must
   * not turn `user.db` into a second copy of the mail. `sha256` is what lets the send path notice a
   * file changed underneath it, so it is carried when known and `null` when not — design/07 §9 says
   * a missing hash is filled in before sending, not that the draft is refused.
   */
  function serializeAttachments(files) {
    return JSON.stringify(
      (files ?? []).map((file) => ({
        name: file.name ?? '',
        mime: file.mime ?? 'application/octet-stream',
        path: file.path ?? '',
        size: file.size ?? 0,
        sha256: file.sha256 ?? null,
      }))
    );
  }

  function parseAttachments(json) {
    let parsed;
    try {
      parsed = JSON.parse(json ?? '[]');
    } catch {
      parsed = null;
    }
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter((file) => file && typeof file.path === 'string')
      .map((file) => ({
        name: file.name ?? '',
        mime: file.mime ?? 'application/octet-stream',
        path: file.path,
        size: file.size ?? 0,
        sha256: file.sha256 ?? null,
      }));
  }

  /**
   * An id for a new draft.
   *
   * Not `crypto.randomUUID()`: whether the webview counts as a secure context is a runtime detail
   * of how the assets are served, and a drafts list that throws on the first keystroke is a much
   * worse failure than a timestamp-based id is a weak one. Uniqueness only has to hold per account.
   */
  function newDraftId() {
    return `d${Date.now().toString(36)}${Math.random().toString(36).slice(2, 10)}`;
  }

  global.__easyEmailPayload = {
    attachment,
    outgoingMail,
    draft,
    serializeAttachments,
    parseAttachments,
    newDraftId,
  };
})(typeof window === 'undefined' ? globalThis : window);
