# Third-Party Notices

Easy Email is distributed under the MIT License (see `LICENSE`). It is not a single
statically linked binary: it links the crates below, and the desktop bundles below
statically embed SQLite. This file records every third-party component, its licence
and, where a distribution obligation exists, what this project does about it.

The lists were generated from the resolved `Cargo.lock` graph on the release commit
(`cargo metadata` + each crate's published manifest). `Both` = reaches the Windows *and*
Linux desktop binaries; `Linux only` / `Windows only` = reached on that platform alone.
Nothing here is vendored or patched: there is no `[patch]` section and no source
replacement stanza in any manifest.

| Crate | Version | Licence | Platforms |
|---|---|---|---|
| `adler2` | 2.0.1 | 0BSD OR MIT OR Apache-2.0 | Both |
| `aes` | 0.8.4 | MIT OR Apache-2.0 | Linux only |
| `aho-corasick` | 1.1.5 | Unlicense OR MIT | Both |
| `alloc-no-stdlib` | 3.0.0 | BSD-3-Clause | Both |
| `alloc-stdlib` | 0.3.0 | BSD-3-Clause | Both |
| `anyhow` | 1.0.104 | MIT OR Apache-2.0 | Both |
| `async-broadcast` | 0.7.2 | MIT OR Apache-2.0 | Linux only |
| `async-channel` | 1.9.0 | Apache-2.0 OR MIT | Both |
| `async-channel` | 2.5.0 | Apache-2.0 OR MIT | Both |
| `async-executor` | 1.14.0 | Apache-2.0 OR MIT | Linux only |
| `async-imap` | 0.12.0 | MIT OR Apache-2.0 | Both |
| `async-io` | 2.6.0 | Apache-2.0 OR MIT | Linux only |
| `async-lock` | 3.4.2 | Apache-2.0 OR MIT | Linux only |
| `async-native-tls` | 0.6.0 | MIT/Apache-2.0 | Both |
| `async-process` | 2.5.0 | Apache-2.0 OR MIT | Linux only |
| `async-recursion` | 1.1.1 | MIT OR Apache-2.0 | Linux only |
| `async-signal` | 0.2.14 | Apache-2.0 OR MIT | Linux only |
| `async-task` | 4.7.1 | Apache-2.0 OR MIT | Linux only |
| `async-trait` | 0.1.92 | MIT OR Apache-2.0 | Linux only |
| `atk` | 0.18.2 | MIT | Linux only |
| `atk-sys` | 0.18.2 | MIT | Linux only |
| `atomic-waker` | 1.1.2 | Apache-2.0 OR MIT | Both |
| `base64` | 0.21.7 | MIT OR Apache-2.0 | Both |
| `base64` | 0.22.1 | MIT OR Apache-2.0 | Both |
| `base64` | 0.23.1 | MIT OR Apache-2.0 | Both |
| `bit-set` | 0.8.0 | Apache-2.0 OR MIT | Both |
| `bit-vec` | 0.8.0 | Apache-2.0 OR MIT | Both |
| `bitflags` | 1.3.2 | MIT/Apache-2.0 | Both |
| `bitflags` | 2.13.2 | MIT OR Apache-2.0 | Both |
| `block-buffer` | 0.10.4 | MIT OR Apache-2.0 | Both |
| `block-padding` | 0.3.3 | MIT OR Apache-2.0 | Linux only |
| `blocking` | 1.7.0 | Apache-2.0 OR MIT | Linux only |
| `brotli` | 9.0.0 | BSD-3-Clause AND MIT | Both |
| `brotli-decompressor` | 6.0.1 | BSD-3-Clause/MIT | Both |
| `bytemuck` | 1.25.2 | Zlib OR Apache-2.0 OR MIT | Both |
| `byteorder` | 1.5.0 | Unlicense OR MIT | Both |
| `byteorder-lite` | 0.1.0 | Unlicense OR MIT | Both |
| `bytes` | 1.12.1 | MIT | Both |
| `cairo-rs` | 0.18.5 | MIT | Linux only |
| `cairo-sys-rs` | 0.18.2 | MIT | Linux only |
| `camino` | 1.2.6 | MIT OR Apache-2.0 | Both |
| `cargo-platform` | 0.1.9 | MIT OR Apache-2.0 | Both |
| `cargo_metadata` | 0.19.2 | MIT | Both |
| `cbc` | 0.1.2 | MIT OR Apache-2.0 | Linux only |
| `cfb` | 0.14.0 | MIT | Both |
| `cfg-if` | 1.0.5 | MIT OR Apache-2.0 | Both |
| `chrono` | 0.4.45 | MIT OR Apache-2.0 | Both |
| `cipher` | 0.4.4 | MIT OR Apache-2.0 | Linux only |
| `concurrent-queue` | 2.5.0 | Apache-2.0 OR MIT | Both |
| `cookie` | 0.18.2 | MIT OR Apache-2.0 | Both |
| `core_detect` | 1.0.0 | MIT/Apache-2.0 | Both |
| `cpufeatures` | 0.2.17 | MIT OR Apache-2.0 | Both |
| `cpufeatures` | 0.3.1 | MIT OR Apache-2.0 | Both |
| `crc32fast` | 1.5.2 | MIT OR Apache-2.0 | Both |
| `crossbeam-channel` | 0.5.17 | MIT OR Apache-2.0 | Both |
| `crossbeam-utils` | 0.8.23 | MIT OR Apache-2.0 | Both |
| `crypto-common` | 0.1.7 | MIT OR Apache-2.0 | Both |
| `cssparser` | 0.37.0 | MPL-2.0 | Both |
| `cssparser-macros` | 0.7.1 | MPL-2.0 | Both |
| `ctor` | 1.0.13 | Apache-2.0 OR MIT | Both |
| `darling` | 0.24.1 | MIT | Both |
| `darling_core` | 0.24.1 | MIT | Both |
| `darling_macro` | 0.24.1 | MIT | Both |
| `dbus` | 0.9.12 | Apache-2.0/MIT | Linux only |
| `dbus-secret-service` | 4.1.0 | MIT OR Apache-2.0 | Linux only |
| `deranged` | 0.5.8 | MIT OR Apache-2.0 | Both |
| `derive_more` | 2.1.1 | MIT | Both |
| `derive_more-impl` | 2.1.1 | MIT | Both |
| `digest` | 0.10.7 | MIT OR Apache-2.0 | Both |
| `dirs` | 7.0.0 | MIT OR Apache-2.0 | Both |
| `dirs-sys` | 0.5.0 | MIT OR Apache-2.0 | Both |
| `displaydoc` | 0.2.7 | MIT OR Apache-2.0 | Both |
| `dlopen2` | 0.8.2 | MIT | Linux only |
| `dlopen2_derive` | 0.4.3 | MIT | Linux only |
| `dom_query` | 0.28.0 | MIT | Both |
| `dpi` | 0.1.2 | Apache-2.0 AND MIT | Both |
| `dtoa` | 1.0.11 | MIT OR Apache-2.0 | Both |
| `dtoa-short` | 0.3.5 | MPL-2.0 | Both |
| `dunce` | 1.0.5 | CC0-1.0 OR MIT-0 OR Apache-2.0 | Both |
| `dyn-clone` | 1.0.20 | MIT OR Apache-2.0 | Both |
| `email-encoding` | 0.4.2 | MIT OR Apache-2.0 | Both |
| `email_address` | 0.2.9 | MIT | Both |
| `encoding_rs` | 0.8.42 | (Apache-2.0 OR MIT) AND BSD-3-Clause | Both |
| `endi` | 1.1.1 | MIT | Linux only |
| `enumflags2` | 0.7.12 | MIT OR Apache-2.0 | Linux only |
| `enumflags2_derive` | 0.7.12 | MIT OR Apache-2.0 | Linux only |
| `equivalent` | 1.0.2 | Apache-2.0 OR MIT | Both |
| `erased-serde` | 0.4.10 | MIT OR Apache-2.0 | Both |
| `errno` | 0.3.14 | MIT OR Apache-2.0 | Linux only |
| `event-listener` | 2.5.3 | Apache-2.0 OR MIT | Both |
| `event-listener` | 5.4.2 | Apache-2.0 OR MIT | Both |
| `event-listener-strategy` | 0.5.4 | Apache-2.0 OR MIT | Both |
| `fallible-iterator` | 0.3.0 | MIT/Apache-2.0 | Both |
| `fallible-streaming-iterator` | 0.1.9 | MIT/Apache-2.0 | Both |
| `fastrand` | 2.5.0 | Apache-2.0 OR MIT | Both |
| `fdeflate` | 0.3.7 | MIT OR Apache-2.0 | Both |
| `field-offset` | 0.3.6 | MIT OR Apache-2.0 | Linux only |
| `flate2` | 1.1.10 | MIT OR Apache-2.0 | Both |
| `fnv` | 1.0.7 | Apache-2.0 / MIT | Both |
| `foldhash` | 0.2.0 | Zlib | Both |
| `foreign-types` | 0.3.2 | MIT/Apache-2.0 | Linux only |
| `foreign-types` | 0.5.0 | MIT/Apache-2.0 | Linux only |
| `foreign-types-shared` | 0.1.1 | MIT/Apache-2.0 | Linux only |
| `foreign-types-shared` | 0.3.1 | MIT/Apache-2.0 | Linux only |
| `form_urlencoded` | 1.2.2 | MIT OR Apache-2.0 | Both |
| `futures-channel` | 0.3.34 | MIT OR Apache-2.0 | Both |
| `futures-core` | 0.3.34 | MIT OR Apache-2.0 | Both |
| `futures-executor` | 0.3.34 | MIT OR Apache-2.0 | Linux only |
| `futures-io` | 0.3.34 | MIT OR Apache-2.0 | Both |
| `futures-lite` | 2.6.1 | Apache-2.0 OR MIT | Linux only |
| `futures-macro` | 0.3.34 | MIT OR Apache-2.0 | Both |
| `futures-task` | 0.3.34 | MIT OR Apache-2.0 | Both |
| `futures-util` | 0.3.34 | MIT OR Apache-2.0 | Both |
| `gdk` | 0.18.2 | MIT | Linux only |
| `gdk-pixbuf` | 0.18.5 | MIT | Linux only |
| `gdk-pixbuf-sys` | 0.18.0 | MIT | Linux only |
| `gdk-sys` | 0.18.2 | MIT | Linux only |
| `gdkwayland-sys` | 0.18.2 | MIT | Linux only |
| `gdkx11` | 0.18.2 | MIT | Linux only |
| `gdkx11-sys` | 0.18.2 | MIT | Linux only |
| `generic-array` | 0.14.7 | MIT | Both |
| `getrandom` | 0.2.17 | MIT OR Apache-2.0 | Both |
| `getrandom` | 0.3.4 | MIT OR Apache-2.0 | Both |
| `getrandom` | 0.4.3 | MIT OR Apache-2.0 | Both |
| `gio` | 0.18.4 | MIT | Linux only |
| `gio-sys` | 0.18.1 | MIT | Linux only |
| `glib` | 0.18.5 | MIT | Linux only |
| `glib-macros` | 0.18.5 | MIT | Linux only |
| `glib-sys` | 0.18.1 | MIT | Linux only |
| `glob` | 0.3.4 | MIT OR Apache-2.0 | Both |
| `gobject-sys` | 0.18.0 | MIT | Linux only |
| `gtk` | 0.18.2 | MIT | Linux only |
| `gtk-sys` | 0.18.2 | MIT | Linux only |
| `gtk3-macros` | 0.18.2 | MIT | Linux only |
| `hashbrown` | 0.12.3 | MIT OR Apache-2.0 | Both |
| `hashbrown` | 0.16.1 | MIT OR Apache-2.0 | Both |
| `hashbrown` | 0.17.1 | MIT OR Apache-2.0 | Both |
| `hashify` | 0.2.9 | Apache-2.0 OR MIT | Both |
| `hashlink` | 0.12.2 | MIT OR Apache-2.0 | Both |
| `heck` | 0.4.1 | MIT OR Apache-2.0 | Both |
| `heck` | 0.5.0 | MIT OR Apache-2.0 | Both |
| `hex` | 0.4.3 | MIT OR Apache-2.0 | Linux only |
| `hkdf` | 0.12.4 | MIT OR Apache-2.0 | Linux only |
| `hmac` | 0.12.1 | MIT OR Apache-2.0 | Linux only |
| `hostname` | 0.4.2 | MIT | Both |
| `html5ever` | 0.39.0 | MIT OR Apache-2.0 | Both |
| `http` | 1.5.0 | MIT OR Apache-2.0 | Both |
| `http-body` | 1.1.0 | MIT | Both |
| `http-body-util` | 0.1.5 | MIT | Both |
| `httparse` | 1.10.1 | MIT OR Apache-2.0 | Both |
| `httpdate` | 1.0.3 | MIT OR Apache-2.0 | Both |
| `hyper` | 1.11.1 | MIT | Both |
| `hyper-rustls` | 0.27.10 | Apache-2.0 OR ISC OR MIT | Both |
| `hyper-util` | 0.1.21 | MIT | Both |
| `iana-time-zone` | 0.1.65 | MIT OR Apache-2.0 | Linux only |
| `ico` | 0.5.0 | MIT | Both |
| `icu_collections` | 2.3.0 | Unicode-3.0 | Both |
| `icu_locale_core` | 2.3.0 | Unicode-3.0 | Both |
| `icu_normalizer` | 2.3.0 | Unicode-3.0 | Both |
| `icu_normalizer_data` | 2.3.0 | Unicode-3.0 | Both |
| `icu_properties` | 2.3.0 | Unicode-3.0 | Both |
| `icu_properties_data` | 2.3.0 | Unicode-3.0 | Both |
| `icu_provider` | 2.3.1 | Unicode-3.0 | Both |
| `ident_case` | 1.0.1 | MIT/Apache-2.0 | Both |
| `idna` | 1.1.0 | MIT OR Apache-2.0 | Both |
| `idna_adapter` | 1.2.2 | Apache-2.0 OR MIT | Both |
| `image` | 0.25.10 | MIT OR Apache-2.0 | Both |
| `imap-proto` | 0.17.0 | MIT OR Apache-2.0 | Both |
| `indexmap` | 1.9.3 | Apache-2.0 OR MIT | Both |
| `indexmap` | 2.14.2 | Apache-2.0 OR MIT | Both |
| `infer` | 0.22.0 | MIT | Both |
| `inout` | 0.1.4 | MIT OR Apache-2.0 | Linux only |
| `ipnet` | 2.12.2 | MIT OR Apache-2.0 | Both |
| `itoa` | 1.0.18 | MIT OR Apache-2.0 | Both |
| `javascriptcore-rs` | 1.1.2 | MIT | Linux only |
| `javascriptcore-rs-sys` | 1.1.1 | MIT | Linux only |
| `json-patch` | 4.2.0 | MIT/Apache-2.0 | Both |
| `jsonptr` | 0.7.1 | MIT OR Apache-2.0 | Both |
| `keyboard-types` | 0.8.3 | MIT OR Apache-2.0 | Both |
| `keyring` | 3.6.3 | MIT OR Apache-2.0 | Both |
| `lettre` | 0.11.23 | MIT | Both |
| `libappindicator` | 0.9.0 | Apache-2.0 OR MIT | Linux only |
| `libappindicator-sys` | 0.9.0 | Apache-2.0 OR MIT | Linux only |
| `libc` | 0.2.189 | MIT OR Apache-2.0 | Both |
| `libdbus-sys` | 0.2.7 | Apache-2.0/MIT | Linux only |
| `libloading` | 0.7.4 | ISC | Linux only |
| `libsqlite3-sys` | 0.38.2 | MIT | Both |
| `linux-raw-sys` | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | Linux only |
| `litemap` | 0.8.3 | Unicode-3.0 | Both |
| `lock_api` | 0.4.14 | MIT OR Apache-2.0 | Both |
| `log` | 0.4.34 | MIT OR Apache-2.0 | Both |
| `mail-parser` | 0.11.9 | Apache-2.0 OR MIT | Both |
| `markup5ever` | 0.39.0 | MIT OR Apache-2.0 | Both |
| `memchr` | 2.8.3 | Unlicense OR MIT | Both |
| `memoffset` | 0.9.1 | MIT | Linux only |
| `mime` | 0.3.17 | MIT OR Apache-2.0 | Both |
| `miniz_oxide` | 0.8.9 | MIT OR Zlib OR Apache-2.0 | Both |
| `miniz_oxide` | 0.9.1 | MIT OR Zlib OR Apache-2.0 | Both |
| `mio` | 1.2.3 | MIT | Both |
| `moxcms` | 0.8.1 | BSD-3-Clause OR Apache-2.0 | Both |
| `muda` | 0.20.0 | Apache-2.0 OR MIT | Both |
| `multiversion_no_op` | 1.0.0 | Apache-2.0 OR MIT | Both |
| `native-tls` | 0.2.18 | MIT OR Apache-2.0 | Both |
| `new_debug_unreachable` | 1.0.6 | MIT | Both |
| `nom` | 8.0.0 | MIT | Both |
| `notify-rust` | 4.18.1 | MIT OR Apache-2.0 | Both |
| `num` | 0.4.3 | MIT OR Apache-2.0 | Linux only |
| `num-bigint` | 0.4.8 | MIT OR Apache-2.0 | Linux only |
| `num-complex` | 0.4.6 | MIT OR Apache-2.0 | Linux only |
| `num-conv` | 0.2.2 | MIT OR Apache-2.0 | Both |
| `num-integer` | 0.1.47 | MIT OR Apache-2.0 | Linux only |
| `num-iter` | 0.1.46 | MIT OR Apache-2.0 | Linux only |
| `num-rational` | 0.4.2 | MIT OR Apache-2.0 | Linux only |
| `num-traits` | 0.2.19 | MIT OR Apache-2.0 | Both |
| `once_cell` | 1.21.4 | MIT OR Apache-2.0 | Both |
| `openssl` | 0.10.81 | Apache-2.0 | Linux only |
| `openssl-macros` | 0.1.1 | MIT/Apache-2.0 | Linux only |
| `openssl-probe` | 0.2.1 | MIT OR Apache-2.0 | Linux only |
| `openssl-sys` | 0.9.117 | MIT | Linux only |
| `option-ext` | 0.2.0 | MPL-2.0 | Both |
| `ordered-stream` | 0.2.0 | MIT OR Apache-2.0 | Linux only |
| `pango` | 0.18.3 | MIT | Linux only |
| `pango-sys` | 0.18.0 | MIT | Linux only |
| `parking` | 2.2.1 | Apache-2.0 OR MIT | Both |
| `parking_lot` | 0.12.5 | MIT OR Apache-2.0 | Both |
| `parking_lot_core` | 0.9.12 | MIT OR Apache-2.0 | Both |
| `percent-encoding` | 2.3.2 | MIT OR Apache-2.0 | Both |
| `phf` | 0.13.1 | MIT | Both |
| `phf_generator` | 0.13.1 | MIT | Both |
| `phf_macros` | 0.13.1 | MIT | Both |
| `phf_shared` | 0.13.1 | MIT | Both |
| `pin-project` | 1.1.13 | Apache-2.0 OR MIT | Both |
| `pin-project-internal` | 1.1.13 | Apache-2.0 OR MIT | Both |
| `pin-project-lite` | 0.2.17 | Apache-2.0 OR MIT | Both |
| `pin-utils` | 0.1.1 | MIT OR Apache-2.0 | Both |
| `piper` | 0.2.5 | MIT OR Apache-2.0 | Linux only |
| `plist` | 1.10.1 | MIT | Both |
| `png` | 0.17.16 | MIT OR Apache-2.0 | Both |
| `png` | 0.18.1 | MIT OR Apache-2.0 | Both |
| `polling` | 3.11.0 | Apache-2.0 OR MIT | Linux only |
| `potential_utf` | 0.1.6 | Unicode-3.0 | Both |
| `powerfmt` | 0.2.0 | MIT OR Apache-2.0 | Both |
| `ppv-lite86` | 0.2.21 | MIT OR Apache-2.0 | Both |
| `precomputed-hash` | 0.1.1 | MIT | Both |
| `proc-macro-crate` | 1.3.1 | MIT OR Apache-2.0 | Linux only |
| `proc-macro-crate` | 2.0.2 | MIT OR Apache-2.0 | Linux only |
| `proc-macro-crate` | 3.5.0 | MIT OR Apache-2.0 | Linux only |
| `proc-macro-error` | 1.0.4 | MIT OR Apache-2.0 | Linux only |
| `proc-macro-error-attr` | 1.0.4 | MIT OR Apache-2.0 | Linux only |
| `proc-macro2` | 1.0.107 | MIT OR Apache-2.0 | Both |
| `pxfm` | 0.1.30 | BSD-3-Clause OR Apache-2.0 | Both |
| `quick-xml` | 0.42.0 | MIT | Both |
| `quote` | 1.0.47 | MIT OR Apache-2.0 | Both |
| `quoted_printable` | 0.5.2 | 0BSD | Both |
| `rand` | 0.10.3 | MIT OR Apache-2.0 | Both |
| `rand` | 0.8.8 | MIT OR Apache-2.0 | Both |
| `rand` | 0.9.5 | MIT OR Apache-2.0 | Both |
| `rand_chacha` | 0.3.1 | MIT OR Apache-2.0 | Both |
| `rand_chacha` | 0.9.0 | MIT OR Apache-2.0 | Both |
| `rand_core` | 0.10.1 | MIT OR Apache-2.0 | Both |
| `rand_core` | 0.6.4 | MIT OR Apache-2.0 | Both |
| `rand_core` | 0.9.5 | MIT OR Apache-2.0 | Both |
| `raw-window-handle` | 0.6.2 | MIT OR Apache-2.0 OR Zlib | Both |
| `regex` | 1.13.1 | MIT OR Apache-2.0 | Both |
| `regex-automata` | 0.4.18 | MIT OR Apache-2.0 | Both |
| `regex-syntax` | 0.8.11 | MIT OR Apache-2.0 | Both |
| `reqwest` | 0.12.28 | MIT OR Apache-2.0 | Both |
| `reqwest` | 0.13.5 | MIT OR Apache-2.0 | Both |
| `rfd` | 0.16.0 | MIT | Both |
| `ring` | 0.17.14 | Apache-2.0 AND ISC | Both |
| `rusqlite` | 0.40.2 | MIT | Both |
| `rustc-hash` | 2.1.3 | Apache-2.0 OR MIT | Both |
| `rustix` | 1.1.5 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | Linux only |
| `rustls` | 0.23.45 | Apache-2.0 OR ISC OR MIT | Both |
| `rustls-pki-types` | 1.15.1 | MIT OR Apache-2.0 | Both |
| `rustls-webpki` | 0.103.15 | ISC | Both |
| `ryu` | 1.0.23 | Apache-2.0 OR BSL-1.0 | Both |
| `same-file` | 1.0.6 | Unlicense/MIT | Both |
| `schannel` | 0.1.29 | MIT | Windows only |
| `schemars` | 0.8.22 | MIT | Both |
| `schemars` | 0.9.0 | MIT | Both |
| `schemars` | 1.2.2 | MIT | Both |
| `schemars_derive` | 0.8.22 | MIT | Both |
| `scopeguard` | 1.2.0 | MIT OR Apache-2.0 | Both |
| `selectors` | 0.38.0 | MPL-2.0 | Both |
| `self_cell` | 1.3.0 | Apache-2.0 OR GPL-2.0-only | Both |
| `semver` | 1.0.28 | MIT OR Apache-2.0 | Both |
| `serde` | 1.0.229 | MIT OR Apache-2.0 | Both |
| `serde-untagged` | 0.1.9 | MIT OR Apache-2.0 | Both |
| `serde_core` | 1.0.229 | MIT OR Apache-2.0 | Both |
| `serde_derive` | 1.0.229 | MIT OR Apache-2.0 | Both |
| `serde_derive_internals` | 0.29.1 | MIT OR Apache-2.0 | Both |
| `serde_json` | 1.0.151 | MIT OR Apache-2.0 | Both |
| `serde_repr` | 0.1.21 | MIT OR Apache-2.0 | Both |
| `serde_spanned` | 0.6.9 | MIT OR Apache-2.0 | Both |
| `serde_spanned` | 1.1.1 | MIT OR Apache-2.0 | Both |
| `serde_urlencoded` | 0.7.1 | MIT/Apache-2.0 | Both |
| `serde_with` | 3.24.0 | MIT OR Apache-2.0 | Both |
| `serde_with_macros` | 3.24.0 | MIT OR Apache-2.0 | Both |
| `serialize-to-javascript` | 0.1.2 | MIT OR Apache-2.0 | Both |
| `serialize-to-javascript-impl` | 0.1.2 | MIT OR Apache-2.0 | Both |
| `servo_arc` | 0.4.3 | MIT OR Apache-2.0 | Both |
| `sha2` | 0.10.9 | MIT OR Apache-2.0 | Both |
| `signal-hook-registry` | 1.4.8 | MIT OR Apache-2.0 | Linux only |
| `simd-adler32` | 0.3.10 | MIT | Both |
| `simdutf8` | 0.1.5 | MIT OR Apache-2.0 | Both |
| `siphasher` | 1.0.4 | MIT OR Apache-2.0 | Both |
| `slab` | 0.4.12 | MIT | Both |
| `smallvec` | 1.16.2 | MIT OR Apache-2.0 | Both |
| `socket2` | 0.6.5 | MIT OR Apache-2.0 | Both |
| `softbuffer` | 0.4.8 | MIT OR Apache-2.0 | Windows only |
| `soup3` | 0.5.0 | MIT | Linux only |
| `soup3-sys` | 0.5.0 | MIT | Linux only |
| `stable_deref_trait` | 1.2.1 | MIT OR Apache-2.0 | Both |
| `stop-token` | 0.7.0 | MIT OR Apache-2.0 | Both |
| `string_cache` | 0.9.0 | MIT OR Apache-2.0 | Both |
| `strsim` | 0.11.1 | MIT | Both |
| `subtle` | 2.6.1 | BSD-3-Clause | Both |
| `syn` | 1.0.109 | MIT OR Apache-2.0 | Both |
| `syn` | 2.0.119 | MIT OR Apache-2.0 | Both |
| `syn` | 3.0.6 | MIT OR Apache-2.0 | Both |
| `sync_wrapper` | 1.0.2 | Apache-2.0 | Both |
| `synstructure` | 0.14.0 | MIT | Both |
| `tao` | 0.37.1 | Apache-2.0 | Both |
| `tauri` | 2.12.1 | Apache-2.0 OR MIT | Both |
| `tauri-codegen` | 2.7.1 | Apache-2.0 OR MIT | Both |
| `tauri-macros` | 2.7.1 | Apache-2.0 OR MIT | Both |
| `tauri-plugin-dialog` | 2.8.1 | Apache-2.0 OR MIT | Both |
| `tauri-plugin-fs` | 2.6.0 | Apache-2.0 OR MIT | Both |
| `tauri-plugin-notification` | 2.5.1 | Apache-2.0 OR MIT | Both |
| `tauri-runtime` | 2.12.1 | Apache-2.0 OR MIT | Both |
| `tauri-runtime-wry` | 2.12.1 | Apache-2.0 OR MIT | Both |
| `tauri-utils` | 2.10.1 | Apache-2.0 OR MIT | Both |
| `tauri-winrt-notification` | 0.8.1 | MIT OR Apache-2.0 | Windows only |
| `tendril` | 0.5.1 | MIT OR Apache-2.0 | Both |
| `thiserror` | 1.0.69 | MIT OR Apache-2.0 | Both |
| `thiserror` | 2.0.21 | MIT OR Apache-2.0 | Both |
| `thiserror-impl` | 1.0.69 | MIT OR Apache-2.0 | Both |
| `thiserror-impl` | 2.0.21 | MIT OR Apache-2.0 | Both |
| `time` | 0.3.55 | MIT OR Apache-2.0 | Both |
| `time-core` | 0.1.9 | MIT OR Apache-2.0 | Both |
| `time-macros` | 0.2.32 | MIT OR Apache-2.0 | Both |
| `tinystr` | 0.8.4 | Unicode-3.0 | Both |
| `tokio` | 1.53.1 | MIT | Both |
| `tokio-macros` | 2.7.2 | MIT | Both |
| `tokio-rustls` | 0.26.6 | MIT OR Apache-2.0 | Both |
| `toml` | 0.8.2 | MIT OR Apache-2.0 | Both |
| `toml` | 1.1.6+spec-1.1.0 | MIT OR Apache-2.0 | Both |
| `toml_datetime` | 0.6.3 | MIT OR Apache-2.0 | Both |
| `toml_datetime` | 1.1.1+spec-1.1.0 | MIT OR Apache-2.0 | Both |
| `toml_edit` | 0.19.15 | MIT OR Apache-2.0 | Linux only |
| `toml_edit` | 0.20.2 | MIT OR Apache-2.0 | Linux only |
| `toml_edit` | 0.25.15+spec-1.1.0 | MIT OR Apache-2.0 | Linux only |
| `toml_parser` | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 | Both |
| `toml_writer` | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 | Both |
| `tower` | 0.5.3 | MIT | Both |
| `tower-http` | 0.6.11 | MIT | Both |
| `tower-layer` | 0.3.3 | MIT | Both |
| `tower-service` | 0.3.3 | MIT | Both |
| `tracing` | 0.1.44 | MIT | Both |
| `tracing-attributes` | 0.1.31 | MIT | Linux only |
| `tracing-core` | 0.1.36 | MIT | Both |
| `tray-icon` | 0.25.1 | MIT OR Apache-2.0 | Both |
| `try-lock` | 0.2.5 | MIT | Both |
| `typeid` | 1.0.3 | MIT OR Apache-2.0 | Both |
| `typenum` | 1.20.1 | MIT OR Apache-2.0 | Both |
| `unicode-ident` | 1.0.26 | (MIT OR Apache-2.0) AND Unicode-3.0 | Both |
| `unicode-segmentation` | 1.13.3 | MIT OR Apache-2.0 | Windows only |
| `untrusted` | 0.9.0 | ISC | Both |
| `url` | 2.5.8 | MIT OR Apache-2.0 | Both |
| `urlpattern` | 0.6.0 | MIT | Both |
| `utf8_iter` | 1.0.4 | Apache-2.0 OR MIT | Both |
| `uuid` | 1.26.1 | Apache-2.0 OR MIT | Both |
| `walkdir` | 2.5.0 | Unlicense/MIT | Both |
| `want` | 0.3.1 | MIT | Both |
| `web-time` | 1.1.0 | MIT OR Apache-2.0 | Both |
| `web_atoms` | 0.2.6 | MIT OR Apache-2.0 | Both |
| `webkit2gtk` | 2.0.2 | MIT | Linux only |
| `webkit2gtk-sys` | 2.0.2 | MIT | Linux only |
| `webpki-roots` | 1.0.9 | CDLA-Permissive-2.0 | Both |
| `webview2-com` | 0.39.1 | MIT | Windows only |
| `webview2-com-macros` | 0.8.1 | MIT | Windows only |
| `webview2-com-sys` | 0.39.1 | MIT | Windows only |
| `winapi-util` | 0.1.11 | Unlicense OR MIT | Windows only |
| `window-vibrancy` | 0.8.1 | Apache-2.0 OR MIT | Windows only |
| `windows` | 0.62.2 | MIT OR Apache-2.0 | Windows only |
| `windows-collections` | 0.3.2 | MIT OR Apache-2.0 | Windows only |
| `windows-core` | 0.62.2 | MIT OR Apache-2.0 | Windows only |
| `windows-future` | 0.3.2 | MIT OR Apache-2.0 | Windows only |
| `windows-implement` | 0.60.2 | MIT OR Apache-2.0 | Windows only |
| `windows-interface` | 0.59.3 | MIT OR Apache-2.0 | Windows only |
| `windows-link` | 0.2.1 | MIT OR Apache-2.0 | Windows only |
| `windows-numerics` | 0.3.1 | MIT OR Apache-2.0 | Windows only |
| `windows-registry` | 0.6.1 | MIT OR Apache-2.0 | Windows only |
| `windows-result` | 0.4.1 | MIT OR Apache-2.0 | Windows only |
| `windows-strings` | 0.5.1 | MIT OR Apache-2.0 | Windows only |
| `windows-sys` | 0.45.0 | MIT OR Apache-2.0 | Windows only |
| `windows-sys` | 0.52.0 | MIT OR Apache-2.0 | Windows only |
| `windows-sys` | 0.59.0 | MIT OR Apache-2.0 | Windows only |
| `windows-sys` | 0.60.2 | MIT OR Apache-2.0 | Windows only |
| `windows-sys` | 0.61.2 | MIT OR Apache-2.0 | Windows only |
| `windows-targets` | 0.42.2 | MIT OR Apache-2.0 | Windows only |
| `windows-targets` | 0.52.6 | MIT OR Apache-2.0 | Windows only |
| `windows-targets` | 0.53.5 | MIT OR Apache-2.0 | Windows only |
| `windows-threading` | 0.2.1 | MIT OR Apache-2.0 | Windows only |
| `windows-version` | 0.1.7 | MIT OR Apache-2.0 | Windows only |
| `windows_x86_64_msvc` | 0.42.2 | MIT OR Apache-2.0 | Windows only |
| `windows_x86_64_msvc` | 0.52.6 | MIT OR Apache-2.0 | Windows only |
| `windows_x86_64_msvc` | 0.53.1 | MIT OR Apache-2.0 | Windows only |
| `winnow` | 0.5.40 | MIT | Both |
| `winnow` | 1.0.4 | MIT | Both |
| `writeable` | 0.6.4 | Unicode-3.0 | Both |
| `wry` | 0.57.0 | Apache-2.0 OR MIT | Both |
| `x11` | 2.21.0 | MIT | Linux only |
| `x11-dl` | 2.21.0 | MIT | Linux only |
| `yoke` | 0.8.3 | Unicode-3.0 | Both |
| `yoke-derive` | 0.8.4 | Unicode-3.0 | Both |
| `zbus` | 4.4.0 | MIT | Linux only |
| `zbus` | 5.19.0 | MIT | Linux only |
| `zbus_macros` | 4.4.0 | MIT | Linux only |
| `zbus_macros` | 5.19.0 | MIT | Linux only |
| `zbus_names` | 3.0.0 | MIT | Linux only |
| `zbus_names` | 4.3.4 | MIT | Linux only |
| `zcheapstr` | 1.1.0 | MIT | Linux only |
| `zerocopy` | 0.8.59 | BSD-2-Clause OR Apache-2.0 OR MIT | Both |
| `zerofrom` | 0.1.8 | Unicode-3.0 | Both |
| `zerofrom-derive` | 0.1.8 | Unicode-3.0 | Both |
| `zeroize` | 1.9.0 | Apache-2.0 OR MIT | Both |
| `zeroize_derive` | 1.5.0 | Apache-2.0 OR MIT | Linux only |
| `zerotrie` | 0.2.5 | Unicode-3.0 | Both |
| `zerovec` | 0.11.8 | Unicode-3.0 | Both |
| `zerovec-derive` | 0.11.6 | Unicode-3.0 | Both |
| `zmij` | 1.0.23 | MIT | Both |
| `zvariant` | 4.2.0 | MIT | Linux only |
| `zvariant` | 5.15.0 | MIT | Linux only |
| `zvariant_derive` | 4.2.0 | MIT | Linux only |
| `zvariant_derive` | 5.15.0 | MIT | Linux only |
| `zvariant_utils` | 2.1.0 | MIT | Linux only |
| `zvariant_utils` | 4.2.0 | MIT | Linux only |

## Components bundled inside the binaries

| Component | Licence | Note |
|---|---|---|
| SQLite (amalgamation) | **Public domain** | Statically linked through `rusqlite` with the `bundled` feature. SQLite's authors dedicate it to the public domain; <https://www.sqlite.org/copyright.html>. No attribution is legally required. |
| WebView2 Runtime (Windows) | Microsoft Edge WebView2 Software Licence | Not redistributed: the installer downloads Microsoft's evergreen bootstrapper at install time (`downloadBootstrapper`), so Microsoft's EULA is presented to the end user directly. |
| WebKitGTK (Linux) | LGPL-2.1+ (system library) | Dynamically linked against the distribution's `libwebkit2gtk-4.1-0`; declared as a package dependency, not shipped. |

## Distribution obligations, and how they are met

**Copyleft: none that forces source disclosure.** Five crates are MPL-2.0
(`cssparser`, `cssparser-macros`, `dtoa-short`, `option-ext`, `selectors`). MPL-2.0 is
*file-scoped* copyleft and triggers only if those files are **modified** — they are not
modified, so no source obligation arises. MPL-2.0 §3.2 still asks that recipients be
informed how to obtain the source of those files; this file is that notice (each crate is
an unmodified copy of its published crates.io package at the version above).

**Dual-licensed crates with a copyleft arm — permissive arm elected.**
`self_cell 1.3.0` is `Apache-2.0 OR GPL-2.0-only`. This project elects **Apache-2.0** and
distributes the crate unmodified, so the GPL arm is never exercised.

**Attribution-only licences.** BSD-2/3-Clause, ISC, Zlib, 0BSD, CC0, Unicode-3.0 and CDLA-
Permissive-2.0 crates require reproducing their copyright notice with the distribution —
which is what this file does. The `Unicode-3.0` group is the ICU/IDNA family reached
through `idna`/`url`; Unicode's licence permits use with the notice intact and imposes no
other condition.

**Apache-2.0 NOTICE propagation.** `ring 0.17.14` is `Apache-2.0 AND ISC`; Apache-2.0 §4(d)
requires propagating any upstream `NOTICE` file, so ring's `LICENSE` and notice text are
reproduced by reference at <https://github.com/briansmith/ring/blob/master/LICENSE>
(unmodified upstream source, version pinned above).

**No third-party credentials.** The application contains no OAuth client secrets. Every
provider that requires OAuth (Gmail, Microsoft 365) asks the user for their own
registration; nothing borrowed is baked into any release artifact.

