# devices: copies of Schemify's core MOS symbols

`nmos4.shprim` and `pmos4.shprim` from Schemify's `crates/doc/devices/` at rev
1752960b3906f9b29765222531b5bfe29e8f6542. Generated `tech:` device cells take their pins and
drawing from these byte for byte (`src/install.rs`), so a placed `tech:` instance and a direct
`core:` one look and connect alike. When Schemify changes them, copy them again with the
`rev` bump in `Cargo.toml`.
