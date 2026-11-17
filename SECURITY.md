# Security policy

kvik is at the design stage and has no released versions; the repository holds a specification and a reference
model.

## Crafted files
Every file of a store is read as untrusted input, and a reader's work on a file is bounded whatever its bytes
(`docs/format.md` A, common rules: scan bound). For a file of L bytes, about 2L bytes are checksummed and 16 block
headers are read per 8 bytes. Candidates read in full and rejected may check at most 16 MiB of body per window
(8 MiB in a log segment, 16 MiB in the manifest); past that the rest of the window is passed as damaged, and reported
with the damage around it. Each kept body is checked once, so the body checks of a crafted log segment cost a start
up to about 0.8 s at 64 MiB, and those of a crafted manifest up to about 0.15 s per 16 MiB (measured on a shared
4-core cloud machine), whatever its bodies hold (Q-870). The header reads are bounded in count but not yet in time: a
file with a valid block header every 8 bytes and no body checksum that holds costs about 7.8 s per 64 MiB segment
and 1.3 s per 16 MiB manifest, above the 1 s restart budget, whether it reaches the store directory or a bucket the
store reads from. Bounding that walk so that every crafted file fits the budget is planned (roadmap 4.9e, Q-873).
The fuzz targets of the log segment and the manifest check these counts. Working memory, beyond
the file and the read's results, is bounded by about one window. Write access to the store directory can still
destroy or alter data.

## Reporting a vulnerability
Please report suspected vulnerabilities privately through GitHub's private vulnerability reporting: open the
repository's **Security** tab and choose **Report a vulnerability**. Do not open a public issue.

Include what you found, how to reproduce it, and the impact you expect. We aim to acknowledge reports within seven
days.
