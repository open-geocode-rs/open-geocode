import html
import re
import subprocess
import sys
from collections import Counter, defaultdict
from itertools import groupby
from urllib.parse import quote

import tree_sitter_rust
from tree_sitter import Language, Parser, Query, QueryCursor


RUST = Language(tree_sitter_rust.language())
COMMENTS = Query(RUST, "[(line_comment) (block_comment)] @comment")
HUNK = re.compile(rb"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@", re.MULTILINE)


def git(*args):
    return subprocess.check_output(["git", "--literal-pathspecs", *args])


def comment_text_by_line(parser, source):
    tree = parser.parse(source)
    lines = defaultdict(list)
    for node in sorted(
        QueryCursor(COMMENTS).captures(tree.root_node).get("comment", []),
        key=lambda node: node.start_byte,
    ):
        text_by_line = source[node.start_byte:node.end_byte].removesuffix(b"\n").split(b"\n")
        for line, text in enumerate(text_by_line, node.start_point.row + 1):
            lines[line].append(text)
    return lines


def changed_comments(base, head):
    base = git("merge-base", base, head).decode().strip()
    changes = iter(git("diff", "--raw", "--no-abbrev", "-z", "-M", base, head).split(b"\0")[:-1])
    parser = Parser(RUST)
    diff = ("diff", "--no-color", "--no-ext-diff", "--no-textconv", "--text", "--unified=0")
    for change in changes:
        old_mode, new_mode, old_blob, new_blob, status = change.decode().split()
        path_bytes = next(changes)
        if status.startswith(("R", "C")):
            path_bytes = next(changes)
        if not path_bytes.endswith(b".rs") or not new_mode.startswith("100"):
            continue

        path = path_bytes.decode("utf-8", errors="surrogateescape")
        if not old_mode.startswith(":100"):
            patch = git(*diff, base, head, "--", path)
        else:
            patch = git(*diff, old_blob, new_blob)
        hunks = HUNK.findall(patch)
        if not any(new_count != b"0" for _, _, _, new_count in hunks):
            continue

        before = (
            comment_text_by_line(parser, git("cat-file", "blob", old_blob))
            if old_mode.startswith(":100")
            else {}
        )
        after = comment_text_by_line(parser, git("cat-file", "blob", new_blob))
        touched = []
        for old_start, old_count, new_start, new_count in hunks:
            old_first, new_first = int(old_start), int(new_start)
            old_lines = range(old_first, old_first + int(old_count or b"1"))
            new_lines = range(new_first, new_first + int(new_count or b"1"))
            previous = Counter(text for line in old_lines for text in before.get(line, ()))
            for line in new_lines:
                current = Counter(after.get(line, ()))
                if current - previous:
                    touched.append(line)
                previous -= current
        if touched:
            yield path, touched


def report(base, head, repo_url):
    files = list(changed_comments(base, head))
    if not files:
        return

    count = sum(len(lines) for _, lines in files)
    print(f"### Comment Cop\n\nRust comment lines added or edited · **{count}**\n")
    for path, lines in files:
        path_bytes = path.encode("utf-8", errors="surrogateescape")
        label = html.escape(path_bytes.decode("utf-8", errors="replace").replace("\n", r"\n"))
        label = label.replace("[", "&#91;").replace("]", "&#93;").replace("@", "&#64;")
        file_url = f"{repo_url}/blob/{head}/{quote(path_bytes)}"
        links = []
        for _, group in groupby(enumerate(lines), key=lambda entry: entry[1] - entry[0]):
            first, *rest = [line for _, line in group]
            if rest:
                links.append(f"[L{first}–L{rest[-1]}]({file_url}#L{first}-L{rest[-1]})")
            else:
                links.append(f"[L{first}]({file_url}#L{first})")
        print(f"- <code>{label}</code> · {', '.join(links)}")


if __name__ == "__main__":
    report(*sys.argv[1:])
