#!/usr/bin/env python3
"""Applies the fixes that the compiler's diagnostics state exactly, for example to code written
C#-style (no 'mut', 'own' or [Recursive], one-line bodies without braces).

    python3 tools/autofix.py FILE_OR_PROJECT [--sijoc PATH] [--rounds N]

Each round runs `sijoc check`, and for the errors whose fix is unambiguous edits the source:

  E0310  'x' is immutable / field 'F' is immutable / parameter 'p' is immutable
         -> 'mut' on the declaration
  E0310  'M' changes 'this'                      -> 'mut' on the struct method
  E0403  ... 'M' changes 'this', so 'N' must be declared 'mut' too -> 'mut' on N
  E0505  'M' returns a list                      -> 'own' on its return type
  E0312  'M' calls itself                        -> [Recursive] above it
  E0031  the body of 'if' must be a block        -> braces around a one-line body

It stops when a round fixes nothing; the remaining errors are printed for a person to fix.
Edits are textual and conservative: a declaration it cannot find is left alone.
"""
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SIJOC = os.path.join(HERE, "..", "target", "release", "sijoc")

MODS = r"(?:(?:public|private|protected|static|override|virtual|abstract|async|client|extern(?:\(\"[^\"]*\"\))?)\s+)*"


def run(sijoc, target):
    p = subprocess.run([sijoc, "check", target], capture_output=True, text=True)
    return p.stdout + p.stderr


def parse(out):
    """[(code, message, file, line, col, notes)]"""
    diags = []
    cur = None
    for ln in out.splitlines():
        m = re.match(r"^(error|warning)\[(\w+)\]: (.*)$", ln)
        if m:
            cur = {"code": m.group(2), "msg": m.group(3), "file": None, "line": 0, "col": 0, "notes": []}
            diags.append(cur)
            continue
        if cur is None:
            continue
        m = re.match(r"^\s*--> (.*):(\d+):(\d+)$", ln)
        if m and cur["file"] is None:
            cur["file"], cur["line"], cur["col"] = m.group(1), int(m.group(2)), int(m.group(3))
            continue
        m = re.match(r"^\s*= note: (.*)$", ln)
        if m:
            cur["notes"].append(m.group(1))
    return diags


class Src:
    def __init__(self, path):
        self.path = path
        with open(path) as f:
            self.lines = f.read().split("\n")
        self.dirty = False

    def save(self):
        if self.dirty:
            with open(self.path, "w") as f:
                f.write("\n".join(self.lines))


def insert_mut_local(src, line, name):
    """'mut ' before the nearest declaration of local `name` at or above `line` (1-based)."""
    decl = re.compile(r"(^|[(;{]\s*|^\s*)(?:mut\s+)?((?:var|[A-Za-z_][\w.]*(?:<[^;=()]*?>)?(?:\[\])*\??(?:\s+in\s+-?[\d.]+\.\.-?[\d.]+)?)\s+" + re.escape(name) + r"\s*(?:=|;|,|\)))")
    for i in range(line - 1, max(-1, line - 400), -1):
        text = src.lines[i]
        for m in decl.finditer(text):
            start = m.start(2)
            before = text[:start]
            # `F(ref x)` / `F(out x)` are arguments, not declarations
            if re.match(r"(return|new|is|case|else|ref|out|in)\b", m.group(2)):
                continue
            # the nearest declaration is the one: already 'mut', or one that cannot take it
            if re.search(r"\b(mut|out|ref)\s*$", before) or re.search(r"\bforeach\s*\(\s*$", before):
                return False
            src.lines[i] = text[:start] + "mut " + text[start:]
            src.dirty = True
            return True
    return False


def insert_mut_param(src, line, name):
    pat = re.compile(r"([(,]\s*)((?:own\s+)?[A-Za-z_][\w.]*(?:<[^()]*?>)?(?:\[\])*\??(?:\s+in\s+-?[\d.]+\.\.-?[\d.]+)?\s+" + re.escape(name) + r"\s*[,)])")
    for i in range(line - 1, max(-1, line - 400), -1):
        m = pat.search(src.lines[i])
        if m:
            t = src.lines[i]
            if re.search(r"\bmut\s*$", t[: m.start(2)]):
                return False
            src.lines[i] = t[: m.start(2)] + "mut " + t[m.start(2):]
            src.dirty = True
            return True
    return False


def member_decl(src, name, owner=None):
    """(line index, column after the modifiers) of the declaration of member `name` (field or
    method), inside type `owner` when given; also finds members of one-line declarations"""
    pat = re.compile(r"(^|[{;}]\s*)(\s*(?:\[[^\]]*\]\s*)*" + MODS + r")(?!return\b|new\b)([A-Za-z_][\w.]*(?:<[^;=()]*?>)?(?:\[\])*\??(?:\s+in\s+-?[\d.]+\.\.-?[\d.]+)?)\s+" + re.escape(name) + r"\s*(=|;|\(|<|=>)")
    start = 0
    if owner:
        decl = re.compile(r"\b(?:class|struct|interface)\s+" + re.escape(owner) + r"\b")
        start = next((i for i, t in enumerate(src.lines) if decl.search(t)), 0)
    for i in range(start, len(src.lines)):
        t = src.lines[i]
        # from the owner's line on: skip text before its declaration on that line
        begin = decl.search(t).end() if owner and i == start and decl.search(t) else 0
        for m in pat.finditer(t, begin):
            if m.group(3) in ("return", "new", "else", "ref", "out", "in"):
                continue
            return i, m.end(2)
    return None


def insert_member(src, name, word, owner=None):
    found = member_decl(src, name, owner)
    if not found:
        return False
    i, at = found
    t = src.lines[i]
    if re.search(r"\b" + word + r"\s+$", t[:at]):
        return False
    src.lines[i] = t[:at] + word + " " + t[at:]
    src.dirty = True
    return True


def insert_recursive(src, name, line):
    """[Recursive] above the declaration the error points at (line: its name)"""
    i = line - 1
    if not re.search(r"\b" + re.escape(name) + r"\s*[(<]", src.lines[i]):
        found = member_decl(src, name)
        if not found:
            return False
        i, _ = found
    if i > 0 and "[Recursive]" in src.lines[i - 1]:
        return False
    ind = re.match(r"^\s*", src.lines[i]).group(0)
    src.lines.insert(i, ind + "[Recursive]")
    src.dirty = True
    return True


def brace_body(src, line, col):
    """wrap a one-line body starting at (line, col) in braces, if it ends on that line"""
    i = line - 1
    t = src.lines[i]
    c = col - 1
    body = t[c:]
    # the body ends at the first ';' at depth 0 (one statement on this line)
    depth = 0
    in_str = False
    for k, ch in enumerate(body):
        if ch == '"':
            in_str = not in_str
        if in_str:
            continue
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == ";" and depth == 0:
            stmt = body[: k + 1]
            rest = body[k + 1:]
            src.lines[i] = t[:c] + "{ " + stmt + " }" + rest
            src.dirty = True
            return True
    return False


def fix_round(sijoc, target):
    diags = parse(run(sijoc, target))
    srcs = {}
    fixed = 0
    left = []
    for d in diags:
        if not d["file"]:
            left.append(d)
            continue
        s = srcs.setdefault(d["file"], Src(d["file"]))
        note = " ".join(d["notes"])
        ok = False
        if d["code"] == "E0310":
            m = re.search(r"'(\w+)' changes 'this'", note)
            if m:
                ok = insert_member(s, m.group(1), "mut")
            elif re.search(r"field '(?:(\w+)\.)?(\w+)' is immutable", note):
                m = re.search(r"field '(?:(\w+)\.)?(\w+)' is immutable", note)
                ok = insert_member(s, m.group(2), "mut", m.group(1))
            elif re.search(r"parameter '(\w+)' is immutable", note):
                ok = insert_mut_param(s, d["line"], re.search(r"parameter '(\w+)' is immutable", note).group(1))
            elif re.search(r"'(\w+)' is immutable", note):
                ok = insert_mut_local(s, d["line"], re.search(r"'(\w+)' is immutable", note).group(1))
            elif re.search(r"immutable variable '(\w+)' has no value", d["msg"]):
                ok = insert_mut_local(s, d["line"], re.search(r"immutable variable '(\w+)'", d["msg"]).group(1))
        elif d["code"] == "E0403":
            m = re.search(r"so '(\w+)' must be declared 'mut' too", note)
            if m:
                ok = insert_member(s, m.group(1), "mut")
            elif re.search(r"field '(?:(\w+)\.)?(\w+)' is immutable", note):
                m = re.search(r"field '(?:(\w+)\.)?(\w+)' is immutable", note)
                ok = insert_member(s, m.group(2), "mut", m.group(1))
            elif re.search(r"'(\w+)' is immutable", note):
                ok = insert_mut_local(s, d["line"], re.search(r"'(\w+)' is immutable", note).group(1))
        elif d["code"] == "E0505":
            m = re.match(r"'(\w+)' returns a", d["msg"])
            if m:
                ok = insert_member(s, m.group(1), "own")
        elif d["code"] == "E0312":
            m = re.match(r"'(\w+)' calls itself", d["msg"])
            if m:
                ok = insert_recursive(s, m.group(1), d["line"])
        elif d["code"] == "E0031":
            ok = brace_body(s, d["line"], d["col"])
        if ok:
            fixed += 1
        else:
            left.append(d)
        # line numbers after an inserted [Recursive] line shift: one structural edit per file per round
        if ok and d["code"] == "E0312":
            break
    for s in srcs.values():
        s.save()
    return fixed, left


def main():
    args = sys.argv[1:]
    sijoc = SIJOC
    rounds = 40
    if "--sijoc" in args:
        k = args.index("--sijoc")
        sijoc = args[k + 1]
        del args[k : k + 2]
    if "--rounds" in args:
        k = args.index("--rounds")
        rounds = int(args[k + 1])
        del args[k : k + 2]
    if len(args) != 1:
        print(__doc__)
        sys.exit(2)
    total = 0
    left = []
    for _ in range(rounds):
        n, left = fix_round(sijoc, args[0])
        total += n
        if n == 0:
            break
    print(f"autofix: {total} edit(s)")
    for d in left:
        loc = f"{d['file']}:{d['line']}:{d['col']}" if d["file"] else ""
        print(f"  left: {d['code']} {loc} {d['msg']}")
    sys.exit(1 if any(d["code"].startswith("E") for d in left) else 0)


if __name__ == "__main__":
    main()
