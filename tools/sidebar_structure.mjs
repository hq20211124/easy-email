/**
 * 侧栏的信息架构，按**结构**断言，不按文本 (`design/12` §16.5 A 栏).
 *
 * The rule under test — 写邮件 must not be part of the scroll content — cannot be checked by looking
 * for a string: the button and the class both exist either way, and a regex that counts `<div>`s once
 * survives one refactor and then silently passes on the wrong tree. So this walks the tag stack of
 * `index.html` and asks about the **ancestor chain** of `#btn-compose`, which is the actual question
 * ("is this button inside the scrolling element?").
 *
 * The other assertions are the removals that came with it: one entrance to the queue, not two
 * (`design/12` §16.4.2), no 「本地」 group title, and no "还没有保存的搜索" paragraph.
 *
 *     node tools/sidebar_structure.mjs
 */

import { readFileSync } from 'node:fs';

const html = readFileSync(new URL('../src/index.html', import.meta.url), 'utf8');
const app = readFileSync(new URL('../src/app.js', import.meta.url), 'utf8');

function fail(message) {
  console.error(`- ${message}`);
  process.exit(1);
}

const VOID = new Set(['area', 'base', 'br', 'col', 'embed', 'hr', 'img', 'input', 'link', 'meta', 'source', 'track', 'wbr']);

/** The open-element stack at the point `id` appears, outermost first. */
function ancestorsOf(id) {
  const stack = [];
  const tag = /<(\/?)([a-zA-Z][\w-]*)([^>]*)>/g;
  let match;
  while ((match = tag.exec(html)) !== null) {
    const [, closing, name, attrs] = match;
    const lower = name.toLowerCase();
    if (closing) {
      // Pop to the matching element; tolerate sloppy nesting rather than throwing, because the point
      // is to answer a question about the tree, not to lint the document.
      const at = stack.map((entry) => entry.name).lastIndexOf(lower);
      if (at >= 0) stack.length = at;
      continue;
    }
    if (new RegExp(`\\bid\\s*=\\s*["']${id}["']`).test(attrs)) {
      return stack.slice();
    }
    if (VOID.has(lower) || attrs.trim().endsWith('/')) continue;
    stack.push({ name: lower, attrs });
  }
  return null;
}

// 1. 写邮件 is not inside the scroll container.
const composeAncestors = ancestorsOf('btn-compose');
if (composeAncestors === null) fail('#btn-compose is missing');
const inScroll = composeAncestors.some((entry) => /\bsidebar-scroll\b/.test(entry.attrs));
if (inScroll) {
  fail('#btn-compose is inside .sidebar-scroll — the primary action would scroll away (design/12 §16.4.4)');
}
const inSidebar = composeAncestors.some((entry) => entry.name === 'aside');
if (!inSidebar) fail('#btn-compose must still live in the sidebar');

// 2. One entrance to the queue: the sidebar's 发件箱 row and its badge are gone.
for (const id of ['btn-outbox', 'outbox-count']) {
  if (html.includes(`id="${id}"`)) {
    fail(`#${id} still exists — the queue is reached through 「已发送」's 待发 tab (design/12 §16.4.2)`);
  }
}

// 3. No 「本地」 group title: those rows were settings, and the heading said "not a folder".
if (/class="section-title"[^>]*>\s*本地\s*</.test(html)) {
  fail('the 「本地」 section title is still there — nothing the user thinks in is "local"');
}

// 4. An empty 保存的搜索 says nothing at all.
if (app.includes('还没有保存的搜索')) {
  fail('「还没有保存的搜索」 is back: an empty section must be hidden, not explained (design/12 §16.4.5)');
}

// 5. And the sections that must exist are still reachable.
for (const id of ['btn-compose', 'btn-drafts', 'mailbox-list', 'btn-about']) {
  if (!html.includes(`id="${id}"`)) fail(`#${id} disappeared from the sidebar`);
}

console.log('写邮件在固定头里，队列只有「已发送」一个入口，空的分组不再占位');
