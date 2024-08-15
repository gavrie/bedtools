A very early stab at implementing a [bedtools](https://bedtools.readthedocs.io/en/latest/) alternative using [noodles](https://crates.io/crates/noodles) and [SQLite](https://www.sqlite.org).
The goal is to be simple and performant: After all SQL is highly specialized in set operations, and Rust with `noodles` provides a performant environment for handling VCF/BED files.
