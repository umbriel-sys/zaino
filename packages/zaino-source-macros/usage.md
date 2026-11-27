# `zaino-source-macros` — usage

One attribute macro, used only by [`zaino-source`](../zaino-source/usage.md):
`#[resilient_port]`. It exists so the retrying form of every validator port
is derived from the single-attempt form instead of written twice.

## What it emits

Placed on a `OneShotX` trait, it emits:

1. the `OneShotX` trait, unchanged;
2. a twin trait `X` (the name with `OneShot` stripped) with the same methods,
   returning `SourceError<E>` where the original returns `QueryError<E>`;
3. `impl X for ValidatorClient<V> where V: OneShotX`, each method running the
   one-shot call through `ValidatorClient::with_retry`.

```rust,ignore
#[resilient_port]
pub trait OneShotGetBlock: ValidatorSource + Send + Sync {
    fn get_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Block, QueryError<GetBlockError, Self::NonDomain>>> + Send;
}
// also defines `GetBlock`, implemented for `ValidatorClient<V: OneShotGetBlock>`
```

The retry ladder and the `QueryError` → `SourceError` translation are written
once, in `ValidatorClient`, and a twin's signature is never restated.

## Rules

- Every method returns `impl Future<Output = Result<T, QueryError<E, _>>> + Send`.
  Anything else is a compile error naming the method.
- Arguments must be `Clone`: the retry loop may call the delegate more than
  once.
- A port that must not be retried (sending a raw transaction) is left
  without the attribute. Exclusion is the absence of the macro, not a case
  inside it.

Adding a port to `zaino-source` means writing the `OneShot*` trait and
annotating it; see that crate's guide for the port rules themselves.
