# Fixtures copied from Schemify

From Schemify's `crates/cli/tests/fixtures/tech/` at rev
1752960b3906f9b29765222531b5bfe29e8f6542: `inv.shcomp` (the `tech:` inverter) and the
generated device cells of `pdk/freepdk45` and `pdk/asap7`. Schemify's `crates/cli/tests/tech.rs`
netlists those cells against its golden netlists; `generated_cells_are_canonical_core_style_symbols`
checks this plugin still generates them byte for byte. After a `SCHEMIFY_BLESS=1` re-bless here,
copy the changed cells back into Schemify.
