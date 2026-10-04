"""Every `#id` a stylesheet styles must still exist somewhere.

Written after a real layout break (M1a-67): the three-column grid moved from a wrapper element back to
`#app`, the wrapper was deleted from the markup, and its `#shell { grid-template-columns: … }` rule was
left behind. Nothing failed — CSS is happy to style an element that does not exist — and the app
silently became a single column with the sidebar squashed into a sliver. A screenshot caught it; the
gate did not. This is the gate catching it.

Ids may come from `index.html` or from any script in `src/` (the renderer builds plenty of elements at
runtime). Anything else is a rule that can no longer apply to anything, which is always either a
mistake or dead code.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CSS = os.path.join(ROOT, "src", "style.css")
HTML = os.path.join(ROOT, "src", "index.html")
SCRIPTS = os.path.join(ROOT, "src")

# `#foo` at the start of a compound selector, or after a comma/space — but not `#fff` in a colour and
# not `url(#x)`.
SELECTOR = re.compile(r"(?:^|[\s,>+~])(#[A-Za-z][\w-]*)", re.MULTILINE)


def read(path):
    with open(path, encoding="utf-8", errors="replace") as handle:
        return handle.read()


css = read(CSS)
markup = read(HTML)
script_text = "\n".join(
    read(os.path.join(SCRIPTS, name))
    for name in os.listdir(SCRIPTS)
    if name.endswith(".js") or name.endswith(".mjs")
)

known = set(re.findall(r'id="([^"]+)"', markup)) | set(re.findall(r"id=['\"]([^'\"]+)['\"]", script_text))
known |= set(re.findall(r"getElementById\(['\"]([^'\"]+)['\"]\)", script_text))

# A hex colour and an id look alike to a regex (`#fff`, `#dc2626`), so hex-shaped tokens are skipped.
# The cost is that an element whose id happens to be `face` or `abc` would be ignored here; no id in
# this app is, and the alternative — parsing CSS properly — is not worth a dependency.
HEX = re.compile(r"^[0-9a-fA-F]{3,8}$")

missing = sorted(
    {
        match.group(1)[1:]
        for match in SELECTOR.finditer(css)
        if not HEX.match(match.group(1)[1:])
    }
    - known
)

if missing:
    for name in missing:
        print(f"- src/style.css styles `#{name}`, which no element in src/ ever has")
    sys.exit(1)

print("css ids: every styled id exists in the markup or is created by a script")
