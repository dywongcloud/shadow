# shadw-core (vendored)

Upstream: https://github.com/SolutionsAsService/shadw-core, commit
`8fd03c563f1dda3c2c2c4547a7101d0fd33cb43b`, crate `crates/shadw-core`.

- `src/` and `LICENSE` are copied unchanged.
- `Cargo.toml` is new. The upstream manifest inherits its fields from that repository's workspace, so this one
  states them directly, with the same versions.

It must run on the patched `hypercore` in `vendor/hypercore`. That copy is upstream 0.16.0 plus three persistence fixes;
see its `SHADW_PATCHES.md`. Without them, a reopened store appends over its own data from offset 8192. The workspace
`[patch.crates-io]` entry enforces this.

To update: re-copy `src/` from a newer shadw-core commit, re-diff `vendor/hypercore` against that repository's copy,
and bump the commit above.
