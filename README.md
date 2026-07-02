# Trino / Presto Connector

Adds Trino and Presto connectivity as an installable connector extension.

This connector is listed in the public Irodori extension marketplace.

## Connector

- Extension ID: `irodori.trino-presto`
- Engine ID: `trinoPresto`
- Wire: `jdbc`
- Default port: `8080`
- Native ABI: `irodori.connector.native.v1`
- Driver linked: `true`

The native driver uses the Trino/Presto HTTP statement API.

Connector metadata lives in `connector.config.json` and `irodori.extension.json`.
The Rust code keeps native ABI exports in `src/lib.rs`, shared buffer/JSON helpers in `src/abi.rs`, and Trino/Presto behavior in `src/driver.rs`.

## Connection Metadata

- Endpoint modes: `hostPort`, `connectionString`
- Transport modes: `direct`, `sshTunnel`, `socks5Proxy`, `httpConnectProxy`, `proxyChain`
- TLS supported: `true`
- Custom driver options: `true`

| Auth method | Label | Secret purposes |
|---|---|---|
| `none` | No authentication | none |
| `connectionString` | Connection string / DSN | none |
| `userPassword` | User/password | `password` |
| `basic` | Basic authentication | `password` |
| `bearerToken` | Bearer token | `token` |
| `oauth2` | OAuth 2.0 | `token` |
| `kerberos` | Kerberos / GSSAPI | `token` |
| `ldap` | LDAP user/password | `password` |
| `clientCertificate` | Client certificate / mTLS | `privateKey`, `privateKeyPassphrase` |
| `customDriverOptions` | Custom driver options | `password`, `token`, `privateKey`, `privateKeyPassphrase` |

## ABI Calls

The driver handles these JSON requests today:

| Method | Response |
|---|---|
| `health` / `ping` | Connector health, engine id, ABI version, and driver link status. |
| `describe` / `capabilities` | Embedded manifest and connector config. |
| `manifest` | Raw `irodori.extension.json`. |
| `config` | Raw `connector.config.json`. |
| `connect` | Opens an HTTP client and reads `/v1/info`. |
| `query` | Runs SQL through `/v1/statement` and follows `nextUri`. |
| `metadata` | Loads table metadata from `information_schema.columns`. |
| `close` | Removes the cached native connection. |

## Development


Generated extension repositories share `../target` across sibling repositories so Rust dependencies are compiled once per checkout. DuckDB and MotherDuck are driver-linked by default; set `IRODORI_CONNECTOR_LINK_DUCKDB=0` only when you need metadata-only DuckDB-compatible scaffolds.


```sh
make check
make build
```

Release packages place platform-specific native artifacts under `dist/native`.
