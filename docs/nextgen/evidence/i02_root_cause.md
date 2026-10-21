# I02 root cause: why a line index would not have paid

Measured (t06_bounded_io): same `limit=10`, a 100-byte file costs 63.8us, a 2 MiB file 110.4ms
(1732x). That number is real and reproducible.

But reading tools/read.rs shows the 110ms is not line scanning. The read path already streams the
file in 8 KiB chunks (`BufReader::with_capacity(16*1024)`, `read(&mut chunk)`), so there is no
line-by-line re-scan to remove. The cost is:

1. reading all 2 MiB (the filesystem read itself), and
2. computing the whole-file SHA256 that the read tool reports as file identity (`MAX_SHA256_FILE_BYTES`
   is 4 MiB, so a 2 MiB file is hashed in full) plus the NUL/binary check that must also see every
   byte.

A line index (offset -> byte offset) would let a later read seek straight to the requested window, but
it cannot skip (1) or (2) without changing what the tool reports: the SHA256 is over the whole file and
the binary check is over the whole file. So the honest reading of the evidence is that the 1732x is an
I/O + hashing cost, and a version-keyed line index would shave only the parse step, not the dominant
term.

That is why the two implementation attempts were reverted rather than shipped: they added a seek path
plus a cache while the measured cost stayed in the part they could not touch. The correct change, if
this is ever reopened, is to make SHA256 optional/lazy for short window reads (a semantic change that
needs the tool contract updated), not to add a line index.
