Generate a Discord-ready "What's New" changelog from recent git history.

**Input:** `$ARGUMENTS` — either a date (e.g. "May 7", "2026-05-07", "May 7 1pm MST") or a commit ref (e.g. "abc1234", "v0.1.2"). If empty, default to the last 7 days.

**Step 0: Sync first.** Run `git fetch origin` so the range is computed against current `origin/main`, not a stale local ref. Use `origin/main` (or whichever branch the user names) as the tip in all `git log` invocations below.

**Step 1: Determine the git log range.**
- If the argument looks like a date/time, convert it **yourself** to a fully-qualified ISO 8601 timestamp with a numeric UTC offset (e.g. `2026-05-11T16:30:00-07:00`) before passing it to `git`. Do **not** hand `git --since` a string containing a named timezone abbreviation like `MST`/`MDT`/`PST` — `git`'s date parser silently mishandles many of these and will quietly truncate the range. If no timezone is given in the input, assume Mountain Time: `-07:00` for MDT (Mar–Nov) or `-07:00`/`-06:00` as appropriate — when unsure, state which offset you used.
- If the argument looks like a commit hash or tag, use `<ref>..origin/main`.
- If empty, use `--since="7 days ago"`.
- Exclude merge commits (`--no-merges`).

**Step 2: Read the commits — and sanity-check the count.**
- First run `git log --no-merges <range> --oneline origin/main | cat` and note how many commits came back. If a date-based range returns suspiciously few commits (e.g. only today's work when you asked for "since yesterday afternoon"), the timezone conversion in Step 1 went wrong — recompute the offset and re-run before proceeding. `git log` truncates long output by default, so always pipe through `| cat` (or use `--no-pager`) to see the full list.
- Then fetch subject lines and bodies for the full range (`--format="%H %s%n%b---" | cat`) and read through every commit to understand the full scope of changes. Cross-reference against the `--oneline` count from the previous step so nothing is dropped.

**Step 3: Synthesize into user-facing changelog.**
Group related commits and distill into a hyphen-prefixed list for Discord. Follow these rules:
- **User-facing language only.** Describe what players/users can now do, not internal implementation details. "Planeswalkers can now activate loyalty abilities at instant speed" not "Support instant-speed loyalty permissions".
- **Consolidate related commits.** Multiple commits that build toward one feature become one bullet. Don't mirror commits 1:1.
- **Skip internal-only changes.** Omit refactors, CI changes, feed refreshes, and code cleanup unless they have user-visible impact.
- **Concrete examples help.** Add parenthetical card names or mechanic names when they clarify what changed (e.g. "e.g. Teferi, Master of Time").
- **Keep it scannable.** Each bullet should be one line, two at most. No headers, no categories — just a flat list.
- **Order by impact.** Lead with the most exciting or broadly-applicable changes.

**Step 4: Output the result.**
Present the changelog in a single fenced code block so the user can copy-paste it directly into Discord. Do not add any preamble inside the code block — just the hyphen-prefixed list.
