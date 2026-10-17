# Window3D: notes for Claude

## Write down every fix

After fixing an issue (a bug the user reported, or one found while testing), add an entry to
`PROBLEMS.md` in the same commit, as a `###` section at the end of the file. Say:

- **Symptom**: what the user saw.
- **Cause**: what was really going on, including how it was found (log lines, scratch tests).
- **Fix**: what changed, with code pointers (`file.rs`, function names) and the commit.
- **How to check**: the log lines or self-test lines that show whether it is back.

Dead ends belong there too, so they aren't tried again. When an entry turns out to be
wrong or out of date, update it instead of adding a contradicting one. `PROBLEMS.md` is the
project's record of how things work and how problems were solved; read the relevant sections
before working on an area.
