# kvik-files

The manifest of the kvik time-series store (`docs/spec.md` §7 Manifest, `docs/format.md` A.14):

- typed manifest records with canonical encoding and strict decoding, golden files and a fuzz target;
- a reader that keeps every valid record on both sides of a corrupt one, and tells a torn tail from a corrupt record;
- the file catalogue: which files are live and retired at each epoch, where they are verified, the losses with their
  acknowledgements, the rebuilds and the `file_id` floors;
- the appender, the one run-time writer of `MANIFEST`: the start's repair, appends after the previous record's sync,
  and the rule that the last record is not acted on irreversibly until a successor is synced;
- the manifest rebuild from the self-describing local files (the procedure of `rebuild_manifest`), the wait while
  deep cannot answer, the provisional rebuild and its holds, and writing a fetched copy of a corrupt record back.

The local files, the verification of an output against its inputs and the losses the scan of the log finds are
injected; the store wires them in roadmap 6.3b. Not published.
