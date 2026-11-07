# maxgame-readiness

Per-hop readiness reporting for `/readyz`, the `GET {BASE_PATH}/v1/ping`
identity route, and the hop probes. It implements the readiness contract,
`CONTRACT.md` in `maxion-game-infra/maxgame-readiness-contract`, which also
holds the aggregator that polls every service's `/readyz`.

Consume it like `../rust`: a path dependency with this repository checked
out as a sibling.

```toml
maxgame-readiness = { path = "../maxgame-admin-guard/readiness", features = ["postgres", "redis"] }
```

| Feature | Default | Adds |
|---|---|---|
| `http` | yes | `PingProbe`, `JwksProbe`, `AuthEndpointProbe`, `ChainIdProbe`, `ReachProbe` |
| `postgres` | no | `PgProbe` and `health_pool` (sqlx 0.8) |
| `redis` | no | `RedisProbe` (redis 1.x; redis 1.8 needs Rust 1.88) |

`schema/readyz.schema.json` is a copy of the contract repository's schema,
kept here so the tests can validate every body they produce. Change both
together.
