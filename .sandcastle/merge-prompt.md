# TASK

Merge the following branches into the current branch:

{{BRANCHES}}

For each branch:

1. Run `git merge <branch> --no-edit`
2. If there are merge conflicts, resolve them intelligently by reading both sides and choosing the correct resolution
3. Stage **only the specific files you resolved conflicts in, by name** (e.g. `git add path/to/file.rs`).
   **Never run `git add -A` or `git add .`.** This working directory has real untracked files sitting in it
   (docs, tooling, scratch output) that must NOT be staged or committed as a side effect of a merge — and if you
   later need to back out of a bad commit, `git reset --hard <ref>` deletes from disk anything that was
   staged/committed but is absent from `<ref>`, so an accidental broad `git add` followed by a reset silently
   destroys those files, not just un-stages them. If you're unsure exactly which files changed, run
   `git diff --name-only` or `git status --porcelain` first and stage that exact list.
4. After resolving conflicts, run `cargo check --features server && cargo test --features server --bin astroburst-server` to verify everything works
5. If tests fail, fix the issues before proceeding to the next branch

After all branches are merged, make a single commit summarizing the merge. Before committing, run
`git status --porcelain` one more time and confirm only the files you intentionally touched are staged — abort
and re-check if anything unexpected (docs, `.sandcastle/*`, other scratch files) shows up staged.

# CLOSE ISSUES

For each branch that was merged, close its issue using the following command:

`gh issue close <ID> --comment "Completed by Sandcastle"`

Here are all the issues:

{{ISSUES}}

Once you've merged everything you can, output <promise>COMPLETE</promise>.
