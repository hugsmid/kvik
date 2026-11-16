# Security policy

kvik is at the design stage and has no released versions; the repository holds a specification and a reference
model.

## Crafted files
Every file of a store is read as untrusted input. A reader's work on a file is linear in its length whatever its
bytes, and its memory is bounded by the largest block the format allows (`docs/format.md` A, common rules: scan
bound); the fuzz targets of the log segment and the manifest check this. One residual case, the checks of
checksum-valid bodies nested in one another, is recorded as Q-230 in `model/OPEN-QUESTIONS.md`. Write access to the
store directory can still destroy or alter data.

## Reporting a vulnerability
Please report suspected vulnerabilities privately through GitHub's private vulnerability reporting: open the
repository's **Security** tab and choose **Report a vulnerability**. Do not open a public issue.

Include what you found, how to reproduce it, and the impact you expect. We aim to acknowledge reports within seven
days.
