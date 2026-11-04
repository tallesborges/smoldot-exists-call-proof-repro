# smoldot `storage::exists` call-proof repro

## Bug

smoldot-light cannot run a runtime call that uses `sp_io::storage::exists` on
a key with a hashed value. (The trie stores a value as a hash when the state
version is 1 and the value is longer than 32 bytes.) The call fails with
`MissingProofEntry`. A full node runs the same call on the same block and
returns a result.

The cause: the full node records the call proof while it runs the call. For
`exists`, it reads only the hash of the value. So the proof contains the hash
but not the value. smoldot handles `ext_storage_exists_version_1` as a read of
the full value, and the value is not in the proof.

Tested with smoldot-light 3.0.0 (latest release) and 2.1.0. Both fail.

## Run

```sh
cargo run --locked   # smoldot-light 3.0.0
cargo run --locked --no-default-features --features smoldot-light-2   # smoldot-light 2.1.0
```

The program connects to the public Paseo relay chain and Paseo Asset Hub Next.
On three finalized blocks, it sends the same `state_call` to smoldot and to the
Asset Hub RPC node. It only reads. It submits nothing.

Exit status:

- `1`: the bug occurred.
- `0`: smoldot and the RPC node returned a result on every block. This is the
  expected result after a fix.
- `2`: a different error. The run is inconclusive.

The first build compiles smoldot with optimizations. It takes several minutes.

## Expected and actual

Expected: smoldot returns the same result as the RPC node.

Actual, the same with both versions:

```text
[1/3] finalized block #2701277 0x9f7096fb256d8c2ef8fa1bfbed04c06d926683d4be8c92b33212c13677a1721b
  smoldot  Core_version: ok
  smoldot  validate_transaction: error {"code":-32000,"message":"Error trying to access the storage required for the runtime call"}
  RPC node validate_transaction: Invalid(Custom(232))
```

With `RUST_LOG=runtime-next-asset-hub-paseo=debug`, each attempt logs
`foreground-runtime-call-progress-invalid-call-proof ... error=MissingProofEntry`.

smoldot bans the peer after each failed attempt, so it can run out of peers.
In some runs, `Core_version` then also failed on the third block, with
`No node available for storage query`. The program then reports the bug on 2
of 3 blocks.

## What the call reads

The call is `TaggedTransactionQueue_validate_transaction`. Its input is an
unsigned `Pgas.claim_pgas` extrinsic with the `AsPgas` transaction extension
(`tx/pgas-claim.hex`).

This extrinsic is already in Asset Hub Next block 2699432. So the runtime now
rejects it with `Invalid(Custom(232))` (already claimed). This does not affect
the repro. Before that check, the validation calls
`asset_exists(2_000_000_000)`. In `pallet-assets`, `asset_exists` is
`Asset::contains_key`, which calls `sp_io::storage::exists`.

The key is `Assets.Asset(2000000000)`:

```text
key   0x682a59d51ab9e48a8c8cc418ff9708d2d34371a193a751eea5883e9553457b2e46de6e688c2258c9eb90a9b5c834b13d00943577
size  190 bytes (state_getStorageSize)
hash  0x4ad43db576545d76a51d4b52845b4ea31b5869d6080f0a8570957718ebe90082 (state_getStorageHash)
```

## Find the missing key

`diagnostics/log-missing-entry.patch` makes smoldot-light 3.0.0 print the key
that it cannot read. To use it, after the first build:

1. Copy `smoldot-light-3.0.0` from `~/.cargo/registry/src/*/` to
   `./smoldot-light`.
2. Run `patch -p1 -d smoldot-light < diagnostics/log-missing-entry.patch`.
3. Add this to `Cargo.toml`:

   ```toml
   [patch.crates-io]
   smoldot-light = { path = "smoldot-light" }
   ```

4. Run `cargo run`.

Every failed attempt prints:

```text
MISSING-PROOF-ENTRY op=StorageGet child_trie=None key=0x682a…3577 node_info=Some(Ok("hash(0x4ad43db576545d76a51d4b52845b4ea31b5869d6080f0a8570957718ebe90082)"))
```

So the proof contains the trie node of the key and the hash of the value
(`StorageValue::HashKnownValueMissing`). It does not contain the value. The
hash is the same as `state_getStorageHash` from the RPC node.

## Cause

Full node (polkadot-sdk), when it builds the call proof:

- `Backend::exists_storage` is `storage_hash(key).is_some()`
  ([`backend.rs`](https://github.com/paritytech/polkadot-sdk/blob/092b50173aa9dbbe6c68e040d075209aee85c5db/substrate/primitives/state-machine/src/backend.rs#L223-L225)).
- `storage_hash` calls `TrieDB::get_hash` with the proof recorder
  ([`trie_backend_essence.rs`](https://github.com/paritytech/polkadot-sdk/blob/092b50173aa9dbbe6c68e040d075209aee85c5db/substrate/primitives/state-machine/src/trie_backend_essence.rs#L482-L493)).
  For a hashed value, `get_hash` does not load the value node. So the proof
  does not record the value.

smoldot:

- `ext_storage_exists_version_1` becomes an `ExternalStorageGet` with
  `max_size: 0`
  ([`host.rs`](https://github.com/paritytech/smoldot/blob/8664c527a7d3a91ded5bbb905cc280a8832ab7fc/lib/src/executor/host.rs#L1082-L1094)).
- `runtime_service` answers every `StorageGet` with
  `call_proof.storage_value(..)`
  ([`runtime_service.rs`](https://github.com/paritytech/smoldot/blob/8664c527a7d3a91ded5bbb905cc280a8832ab7fc/light-base/src/runtime_service.rs#L3615-L3630)).
- `storage_value` returns an error for `HashKnownValueMissing`
  ([`proof_decode.rs`](https://github.com/paritytech/smoldot/blob/8664c527a7d3a91ded5bbb905cc280a8832ab7fc/lib/src/trie/proof_decode.rs#L1066)).
  smoldot turns this error into `MissingProofEntry`. Then it bans the peer
  with `BanSeverity::High` and `BanReason::InvalidCallProof`
  ([`runtime_service.rs`](https://github.com/paritytech/smoldot/blob/8664c527a7d3a91ded5bbb905cc280a8832ab7fc/light-base/src/runtime_service.rs#L2517-L2523)).

## Limits

- The repro uses the public network. It needs internet access, and the peers
  and the RPC node can change. The chain specs in `specs/` are the ones that a
  browser host uses.
- `src/platform.rs` is smoldot-light's `DefaultPlatform` with TLS added for
  `/wss`. `DefaultPlatform` does not dial `/wss` addresses, and all bootnodes
  in these specs use `/wss`.
- The PGAS pallet source is not public. The order of its checks comes from
  that source: origin, call and slot index, then `asset_exists`, then the day,
  the proof, and the claim. The repro does not need a valid claim. It needs
  only that the validation gets to `asset_exists`.
- I did not run smoldot `main` (`8664c527a7`). Its code for this path is the
  same as in 3.0.0.
- I tested only one call on one chain. Other `exists` calls on hashed values
  probably fail in the same way, but I did not test them.
- Tested only on macOS aarch64.

## License

No license is chosen yet. `src/platform.rs` is adapted from smoldot
(GPL-3.0-or-later WITH Classpath-exception-2.0).
