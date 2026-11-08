# mercurio-storage

Persistence for the [Mercurio](https://github.com/frenzox/mercurio) MQTT broker: sessions, subscriptions,
retained messages, wills, in-flight messages and offline queues.

It defines one trait per kind of state, gathered in `MqttStore`, with three
implementations:

- `memory::MemoryStore`: everything in memory, lost on restart.
- `sqlite::SqliteStore`: a SQLite database, behind the `sqlite` feature.
- `postgres::PostgresStore`: a PostgreSQL database, behind the `postgres`
  feature, opened with `PostgresStore::connect(url, ca_path).await`, over
  TLS when the server supports it. `try_lock` and `lock` keep brokers
  sharing the database from serving at the same time.

```rust,ignore
use std::sync::Arc;

use mercurio_server::server::{run_with_storage, ServerConfig};
use mercurio_storage::sqlite::SqliteStore;

let storage = Arc::new(SqliteStore::new("/var/lib/myapp/mqtt.db")?);
run_with_storage(listener, ServerConfig::default(), storage, shutdown).await?;
```

Implement the traits to keep the broker's state anywhere else.

## Features

- `std` (default): standard library support.
- `sqlite`: the SQLite store, with schema migrations applied on open.
- `postgres`: the PostgreSQL store, which creates its tables on connect.

## License

MIT or Apache-2.0, at your option.
