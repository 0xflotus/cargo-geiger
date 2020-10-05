geiger ☢️ 
=========

[![Safety Dance](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

This crate provides some library parts used by [cargo-geiger] that are decoupled
from [cargo].

For more details please see the `README.md` in [cargo-geiger].

Changelog
---------

### 0.4.4
 - Updated dependencies, only patch version updates.

### 0.4.3
 - Updated dependencies.

### 0.4.2
 - Updated dependencies.

### 0.4.1
 - __Bugfix:__ Count all expressions in unsafe functions and nested unsafe scopes [#72],[#71].

### 0.4.0
 - Reduced compile times.
 - Removed walkdir as dependency.
 - Removed `pub fn find_rs_files_in_dir`.

### 0.3.3
 - Updated dependencies.

### 0.3.2
 - Updated dependencies.

### 0.3.1
 - Bugfix for attributes [#57].

### 0.3.0
 - Added a public function to scan source code strings. [#55]

### 0.2.0
 - Scan for `#![forbid(unsafe_code)]`. [#52]

### 0.1.0
 - Parts of `cargo-geiger` has been moved to this crate.

[#52]: https://github.com/rust-secure-code/cargo-geiger/pull/52
[#55]: https://github.com/rust-secure-code/cargo-geiger/pull/55
[#57]: https://github.com/rust-secure-code/cargo-geiger/pull/57
[#71]: https://github.com/rust-secure-code/cargo-geiger/issues/71
[#72]: https://github.com/rust-secure-code/cargo-geiger/pull/72
[cargo-geiger]: https://crates.io/crates/cargo-geiger
[cargo]: https://crates.io/crates/cargo

