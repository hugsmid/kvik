# Security policy

kvik is at the design stage and has no released versions; the repository holds a specification and a reference
model.

## Crafted files
Every file of a store is read as untrusted input, and a reader's work on a file is bounded whatever its bytes
(`docs/format.md` A, common rules: scan bound). For a file of L bytes, about 2L bytes are checksummed and 16 block
headers are read per 8 bytes. Candidates read in full and rejected may check at most 16 MiB of body per window (8 MiB
in a log segment, 16 MiB in the manifest); past that the rest of the window is passed as damaged, and reported with
the damage around it. Each kept body is checked once, so the body checks of a crafted log segment cost a start up to
about 0.8 s at 64 MiB, and those of a crafted manifest up to about 0.15 s per 16 MiB, priced at the dearest rate
measured on a shared 4-core cloud machine, whatever its bodies hold (Q-870). The header walk is bounded in reads
(Q-873): once damage is found a block header's checksum is taken at most twice per 8 bytes, once by the chain ends and
once by a torn tail's commit search, and each block's header is read at most eight times. Its time is measured, not
proven: the dearest crafted 64 MiB log segments built cost a start about 0.25 to 0.61 s, and a crafted manifest about
0.2 to 0.3 s per 16 MiB for its header walk, on the same machine; the cost grows linearly with `log.segment_size` and
with the number of crafted files a start reads. What is promised against the 1 s restart budget, whether a file
reaches the store directory or a bucket the store reads from, is an open question (Q-970). The fuzz targets of the log
segment and the manifest check these counts. Working memory, beyond the file and the read's results, is bounded by
about two windows. Write access to the store directory can still destroy or alter data.

## Reporting a vulnerability
Please report suspected vulnerabilities privately through GitHub's private vulnerability reporting: open the
repository's **Security** tab and choose **Report a vulnerability**. Do not open a public issue.

Include what you found, how to reproduce it, and the impact you expect. We aim to acknowledge reports within seven
days.
