# Security policy

kvik is at the design stage and has no released versions; the repository holds a specification and a reference
model.

## Crafted files
Every file of a store is read as untrusted input, and a reader's work on a file is bounded whatever its bytes
(`docs/format.md` A, common rules: scan bound). For a file of L bytes, about 2L bytes are checksummed and 16 block
headers are read per 8 bytes; the fuzz targets of the log segment and the manifest check both counts. Bodies whose
checksums hold are checked at most about L/8 × w, where w is a fixed window (8 MiB in a log segment, 16 MiB in the
manifest): linear in L, but with a constant large enough that a file crafted by a writer can make a start slow.
Working memory, beyond the file and the read's results, is bounded by about one window. Write access to the store directory can still destroy or alter
data.

## Reporting a vulnerability
Please report suspected vulnerabilities privately through GitHub's private vulnerability reporting: open the
repository's **Security** tab and choose **Report a vulnerability**. Do not open a public issue.

Include what you found, how to reproduce it, and the impact you expect. We aim to acknowledge reports within seven
days.
