# smoldot `storage::exists` call-proof repro

## Bug

smoldot-light fails a runtime call that uses `sp_io::storage::exists` on a key
whose value is stored hashed in the trie (state version 1, value longer than
32 bytes). The error is `MissingProofEntry`, and smoldot bans the peer that
sent the proof. A full node runs the same call on the same block and returns a
result.

The full node builds the call proof while it runs the call. For `exists` it
reads only the value hash, so the proof has the trie node with the hash but not
the value. smoldot handles `ext_storage_exists_version_1` as a read of the full
value, which is not in the proof.

Tested with smoldot-light 3.0.0 (latest release) and 2.1.0. Both fail.

## Run

```sh
cargo run --locked   # smoldot-light 3.0.0
cargo run --locked --no-default-features --features smoldot-light-2   # smoldot-light 2.1.0
```

The program connects to the public Paseo relay chain and Paseo Asset Hub Next.
On three finalized blocks, it calls `state_call` through smoldot and through
the Asset Hub RPC node, with the same block and the same input. It only reads;
it submits nothing.

Exit status `1` means the bug occurred. Exit status `0` means smoldot and the
RPC node both returned a result on every block, which is the expected result
after a fix.

The first build compiles smoldot with optimizations and takes several minutes.

## Expected and actual

Expected: smoldot returns the same result as the RPC node.

Actual (same for both versions):

```text
[1/3] finalized block #2701277 0x9f7096fb256d8c2ef8fa1bfbed04c06d926683d4be8c92b33212c13677a1721b
  smoldot  Core_version: ok
  smoldot  validate_transaction: error {"code":-32000,"message":"Error trying to access the storage required for the runtime call"}
  RPC node validate_transaction: Invalid(Custom(232))
```

With `RUST_LOG=runtime-next-asset-hub-paseo=debug`, each attempt logs
`foreground-runtime-call-progress-invalid-call-proof ... error=MissingProofEntry`.
In two of four runs with 3.0.0, the earlier attempts had banned the peers, and
on the third block even `Core_version` failed with
`No node available for storage query`. The program then reports 2 of 3 blocks.

## What the call reads

The call is `TaggedTransactionQueue_validate_transaction` for an unsigned
`Pgas.claim_pgas` extrinsic with the `AsPgas` transaction extension
(`tx/pgas-claim.hex`). It was included in Asset Hub Next block 2699432, so the
runtime now rejects it with `Invalid(Custom(232))` (already claimed). That does
not matter here: before that check, the validation calls
`asset_exists(2_000_000_000)` on `pallet-assets`, which is
`Asset::contains_key`, which is `sp_io::storage::exists`.

That key is `Assets.Asset(2000000000)`:

```text
key   0x682a59d51ab9e48a8c8cc418ff9708d2d34371a193a751eea5883e9553457b2e46de6e688c2258c9eb90a9b5c834b13d00943577
size  190 bytes (state_getStorageSize)
hash  0x4ad43db576545d76a51d4b52845b4ea31b5869d6080f0a8570957718ebe90082 (state_getStorageHash)
```

`diagnostics/log-missing-entry.patch` makes smoldot-light 3.0.0 print the key
it could not read. With it, every failed attempt prints:

```text
MISSING-PROOF-ENTRY op=StorageGet child_trie=None key=0x682a…3577 node_info=Some(Ok("hash(0x4ad43db576545d76a51d4b52845b4ea31b5869d6080f0a8570957718ebe90082)"))
```

So the proof has the trie node of that key with the value hash
(`StorageValue::HashKnownValueMissing`), and not the value.

To run it: copy `smoldot-light-3.0.0` from `~/.cargo/registry/src/*/` to
`./smoldot-light`, apply the patch with
`patch -p1 -d smoldot-light < diagnostics/log-missing-entry.patch`, add
`[patch.crates-io] smoldot-light = { path = "smoldot-light" }` to `Cargo.toml`,
and run `cargo run`.

## Cause

Full node (polkadot-sdk), on the call-proof request:

- `Backend::exists_storage` is `storage_hash(key).is_some()`
  ([`backend.rs`](https://github.com/paritytech/polkadot-sdk/blob/092b50173aa9dbbe6c68e040d075209aee85c5db/substrate/primitives/state-machine/src/backend.rs#L223-L225)).
- `storage_hash` calls `TrieDB::get_hash` with the proof recorder
  ([`trie_backend_essence.rs`](https://github.com/paritytech/polkadot-sdk/blob/092b50173aa9dbbe6c68e040d075209aee85c5db/substrate/primitives/state-machine/src/trie_backend_essence.rs#L482-L493)).
  For a hashed value, it does not load the value node, so the value is not
  recorded.

smoldot:

- `ext_storage_exists_version_1` becomes an `ExternalStorageGet` with
  `max_size: 0`
  ([`host.rs`](https://github.com/paritytech/smoldot/blob/8664c527a7d3a91ded5bbb905cc280a8832ab7fc/lib/src/executor/host.rs#L1082-L1094)).
- `runtime_service` answers every `StorageGet` with
  `call_proof.storage_value(..)`
  ([`runtime_service.rs`](https://github.com/paritytech/smoldot/blob/8664c527a7d3a91ded5bbb905cc280a8832ab7fc/light-base/src/runtime_service.rs#L3615-L3630)).
- `storage_value` returns an error for `HashKnownValueMissing`
  ([`proof_decode.rs`](https://github.com/paritytech/smoldot/blob/8664c527a7d3a91ded5bbb905cc280a8832ab7fc/lib/src/trie/proof_decode.rs#L1066)),
  which becomes `MissingProofEntry`, and the peer is banned with
  `BanSeverity::High` / `BanReason::InvalidCallProof`
  ([`runtime_service.rs`](https://github.com/paritytech/smoldot/blob/8664c527a7d3a91ded5bbb905cc280a8832ab7fc/light-base/src/runtime_service.rs#L2517-L2523)).

## Limits

- Uses the public network, so it needs internet access, and the peers and RPC
  node can change. The chain specs in `specs/` are the ones a browser host
  uses.
- `src/platform.rs` is smoldot-light's `DefaultPlatform` plus TLS for `/wss`.
  `DefaultPlatform` does not dial `/wss`, and all bootnodes in these specs are
  `/wss`.
- The PGAS pallet source is not public. The order of its checks (origin, call
  and slot index, then `asset_exists`, then the day, the proof, and the claim)
  comes from that source. The repro does not depend on the claim being valid. It depends on
  the validation reaching `asset_exists`.
- smoldot `main` (`8664c527a7`) was not run. Its code for this path is the
  same as in 3.0.0.
- Only one call on one chain was tested. Other `exists` calls on hashed values
  are expected to fail in the same way, but that was not tested.
- Tested only on macOS aarch64.

No license is chosen yet. `src/platform.rs` is adapted from smoldot
(GPL-3.0-or-later WITH Classpath-exception-2.0).
