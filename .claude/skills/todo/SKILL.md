---
name: todo
description: Parse status-tagged todo lines from a markdown file — filter by status, section, or get a quick tally — without loading the whole file.
version: 1.1.0
metadata:
  internal: true
---

# todo — parse status-box todo lines from markdown

When a todo file is long, run the parser to extract just the items you care about instead of loading the whole file.

## Convention

Every todo line carries a status box, optionally after a list marker and indent. Six markers:

```
- [ ]  todo / not started
- [/]  in progress
- [x]  done  (case-insensitive)
- [-]  cancelled / dropped
- [>]  deferred / forwarded
- [?]  question / blocked-on-answer
```

Lines inside fenced code blocks are excluded. Each item is tagged with the nearest enclosing `##`/`###` heading as its section context.

## Invocation

```bash
node scripts/todo/index.mjs <file.md> [flags]
# or equivalently:
nub scripts/todo/index.mjs <file.md> [flags]
```

## Flags

```
--pending, --todo       show [ ] items only
--in-progress, --wip    show [/] items only
--done                  show [x] items only
--cancelled, --dropped  show [-] items only
--deferred              show [>] items only
--question, --blocked   show [?] items only
--not-done              live actionable set: [ ] + [/] + [?]
                        (excludes done, cancelled, and deferred)

--section <substring>   limit to todos under headings matching substring
--counts                print tally (all six categories) and exit
--json                  machine-readable JSON array

--help, -h              usage
```

Flags are combinable. If no status filter is given, all statuses are shown.

## Output format

```
L 7  [ ]  Write contributing guide [Setup]
L11  [ ]  Implement parser [Features]
L12  [/]  Write lexer [Features]
L20  [x]  Handle fenced code blocks [Edge cases]
```

`L<n>` is the 1-based line number — use it with `Read(file, offset=n, limit=1)` to jump straight to a line.

`--counts` output:

```
pending:      5
in-progress:  2
question:     1
deferred:     1
done:         4
cancelled:    1
```

`--not-done` is the fastest way to reconstruct "what's left to act on" after a context reset — `[?]` (questions) count as actionable (answer them); `[>]` deferred and `[-]` cancelled do not.

## Examples

```bash
# Quick status check on a long epic
node scripts/todo/index.mjs epics/v0.1/todo.md --counts

# Everything not yet done in a specific section
node scripts/todo/index.mjs epics/final-polish/todo.md --not-done --section "Work units"

# All in-progress items as JSON (for a sub-agent prompt)
node scripts/todo/index.mjs epics/v0.1/todo.md --wip --json
```

## When to use

- Before loading a long todo file — run `--counts` to see the shape, then `--not-done` to get only the open work.
- When building a sub-agent prompt that needs the open items from one section: `--pending --section <heading>` gives the exact slice without embedding the whole file.
- Combine with `md-toc` when you also need to navigate prose sections of the same file.
